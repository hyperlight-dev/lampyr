// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde_json::json;

use crate::host::Host;
use crate::kube::Kube;

const NAMESPACE: &str = "lampyr-observability";
const MANIFEST: &str = "k8s/observability.yaml";
const DASHBOARD: &str = "k8s/observability-dashboard.json";
const TRACE_ENDPOINT: &str = "http://jaeger.lampyr-observability.svc.cluster.local:4317";
const TRACE_OWNER: &str = "lampyr.dev/observability-trace-endpoint";

#[derive(Clone, Copy)]
pub enum Action {
    Install,
    Status,
    Uninstall,
}

pub fn run(action: Action, context: Option<&str>, host: &Host) -> Result<()> {
    let root = crate::service::platform_root(Path::new("."), None)?;
    let kube = Kube::new(&root, context, host)?;
    match action {
        Action::Install => install(&kube, &root),
        Action::Status => status(&kube),
        Action::Uninstall => uninstall(&kube, &root),
    }
}

fn install(kube: &Kube<'_>, root: &Path) -> Result<()> {
    let manifest = root.join(MANIFEST);
    let dashboard =
        std::fs::read_to_string(root.join(DASHBOARD)).context("read Lampyr Perses dashboard")?;
    let router = kube.optional("lampyr-system", "deployment", "fiber-router")?;
    ensure!(router.is_some(), "Lampyr is not installed in this cluster");
    let router = router.unwrap();
    let owner = router["metadata"]["annotations"][TRACE_OWNER].as_str();
    validate_router_tracing(&router, owner)?;
    kube.apply(&json!({
        "apiVersion": "v1",
        "kind": "Namespace",
        "metadata": {
            "name": NAMESPACE,
            "labels": {"app.kubernetes.io/part-of": "lampyr-observability"}
        }
    }))?;
    kube.apply(&json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": {
            "name": "lampyr-dashboard",
            "namespace": NAMESPACE,
            "labels": {"app.kubernetes.io/managed-by": "lampyr"}
        },
        "data": {"3-lampyr-platform.json": dashboard}
    }))?;
    kube.host
        .verbose_step("Install observability workloads", || {
            kube.host.run(
                kube.command(None)?
                    .args(["apply", "--filename"])
                    .arg(&manifest),
            )
        })?;
    for deployment in ["kube-state-metrics", "prometheus", "jaeger", "perses"] {
        kube.host.run(kube.command(Some(NAMESPACE))?.args([
            "rollout",
            "restart",
            &format!("deployment/{deployment}"),
        ]))?;
        rollout(kube, NAMESPACE, deployment)?;
    }
    if let Err(error) =
        enable_router_tracing(kube).and_then(|()| rollout(kube, "lampyr-system", "fiber-router"))
    {
        if let Err(rollback) = restore_router_tracing(kube, &router) {
            return Err(error).context(format!(
                "fiber-router trace rollback also failed: {rollback:#}"
            ));
        }
        return Err(error);
    }
    println!(
        "observability ready\n\
         dashboard kubectl --context {} --namespace {} port-forward service/perses 8080:8080\n\
         traces kubectl --context {} --namespace {} port-forward service/jaeger 16686:16686\n\
         open http://localhost:8080/projects/lampyr/dashboards/lampyr-platform",
        kube.context, NAMESPACE, kube.context, NAMESPACE
    );
    Ok(())
}

fn status(kube: &Kube<'_>) -> Result<()> {
    println!(
        "{}",
        kube.capture(kube.command(Some(NAMESPACE))?.args([
            "get",
            "deployments,pods,services",
            "--selector",
            "app.kubernetes.io/part-of=lampyr-observability",
        ]))?
    );
    Ok(())
}

fn uninstall(kube: &Kube<'_>, root: &Path) -> Result<()> {
    disable_router_tracing(kube).context("fiber-router did not finish disabling trace export")?;
    kube.delete_manifest(&json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": {"name": "lampyr-dashboard", "namespace": NAMESPACE}
    }))?;
    kube.host
        .verbose_step("Remove observability workloads", || {
            kube.host.run(
                kube.command(None)?
                    .args(["delete", "--filename"])
                    .arg(root.join(MANIFEST))
                    .args(["--ignore-not-found=true", "--wait=true", "--timeout=300s"]),
            )
        })?;
    Ok(())
}

fn enable_router_tracing(kube: &Kube<'_>) -> Result<()> {
    kube.patch(
        "lampyr-system",
        "deployment",
        "fiber-router",
        &json!({"metadata":{"annotations":{TRACE_OWNER:TRACE_ENDPOINT}}}),
    )?;
    let mut command = kube.command(Some("lampyr-system"))?;
    command.args([
        "set",
        "env",
        "deployment/fiber-router",
        "--containers=router",
        &format!("OTEL_EXPORTER_OTLP_ENDPOINT={TRACE_ENDPOINT}"),
        "OTEL_EXPORTER_OTLP_INSECURE=true",
    ]);
    kube.host.run(&mut command)
}

fn owned_router(kube: &Kube<'_>) -> Result<Option<serde_json::Value>> {
    let router = match kube.optional("lampyr-system", "deployment", "fiber-router")? {
        Some(router)
            if router["metadata"]["annotations"][TRACE_OWNER].as_str() == Some(TRACE_ENDPOINT) =>
        {
            router
        }
        _ => return Ok(None),
    };
    validate_router_tracing(&router, Some(TRACE_ENDPOINT))?;
    Ok(Some(router))
}

fn restore_router_tracing(kube: &Kube<'_>, original: &serde_json::Value) -> Result<()> {
    if original["metadata"]["annotations"][TRACE_OWNER].as_str() != Some(TRACE_ENDPOINT) {
        return disable_router_tracing(kube);
    }
    ensure!(
        owned_router(kube)?.is_some(),
        "fiber-router trace ownership changed during installation"
    );
    let mut command = kube.command(Some("lampyr-system"))?;
    command.args([
        "set",
        "env",
        "deployment/fiber-router",
        "--containers=router",
    ]);
    for name in ["OTEL_EXPORTER_OTLP_ENDPOINT", "OTEL_EXPORTER_OTLP_INSECURE"] {
        let argument = match router_env_entry(original, name) {
            Some(entry) => format!(
                "{name}={}",
                entry["value"]
                    .as_str()
                    .context("restore original trace environment")?
            ),
            None => format!("{name}-"),
        };
        command.arg(argument);
    }
    kube.host.run(&mut command)?;
    rollout(kube, "lampyr-system", "fiber-router")
}

fn disable_router_tracing(kube: &Kube<'_>) -> Result<()> {
    if owned_router(kube)?.is_none() {
        return Ok(());
    }
    kube.host.run(kube.command(Some("lampyr-system"))?.args([
        "set",
        "env",
        "deployment/fiber-router",
        "--containers=router",
        "OTEL_EXPORTER_OTLP_ENDPOINT-",
        "OTEL_EXPORTER_OTLP_INSECURE-",
    ]))?;
    rollout(kube, "lampyr-system", "fiber-router")?;
    kube.patch(
        "lampyr-system",
        "deployment",
        "fiber-router",
        &json!({"metadata":{"annotations":{TRACE_OWNER:null}}}),
    )
}

fn router_container(router: &serde_json::Value) -> Option<&serde_json::Value> {
    router["spec"]["template"]["spec"]["containers"]
        .as_array()?
        .iter()
        .find(|container| container["name"] == "router")
}

fn router_env_entry<'a>(
    router: &'a serde_json::Value,
    name: &str,
) -> Option<&'a serde_json::Value> {
    router_container(router)?["env"]
        .as_array()?
        .iter()
        .find(|variable| variable["name"] == name)
}

fn validate_router_tracing(router: &serde_json::Value, owner: Option<&str>) -> Result<()> {
    let env_from = router_container(router).and_then(|container| container["envFrom"].as_array());
    ensure!(
        env_from.is_none_or(Vec::is_empty),
        "fiber-router trace export cannot be verified with envFrom; use explicit environment variables"
    );
    let generic = router_env_entry(router, "OTEL_EXPORTER_OTLP_ENDPOINT");
    let insecure = router_env_entry(router, "OTEL_EXPORTER_OTLP_INSECURE");
    let conflicts = [
        "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
        "OTEL_EXPORTER_OTLP_TRACES_INSECURE",
        "OTEL_SDK_DISABLED",
    ]
    .into_iter()
    .any(|name| router_env_entry(router, name).is_some());
    let ours = owner == Some(TRACE_ENDPOINT)
        && generic.is_none_or(|entry| entry["value"].as_str() == Some(TRACE_ENDPOINT))
        && insecure.is_none_or(|entry| entry["value"].as_str() == Some("true"))
        && !conflicts;
    let empty = owner.is_none() && generic.is_none() && insecure.is_none() && !conflicts;
    ensure!(
        ours || empty,
        "fiber-router trace export is configured outside the Lampyr observability add-on"
    );
    Ok(())
}

fn rollout(kube: &Kube<'_>, namespace: &str, deployment: &str) -> Result<()> {
    kube.host.run(kube.command(Some(namespace))?.args([
        "rollout",
        "status",
        &format!("deployment/{deployment}"),
        "--timeout=300s",
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[test]
    fn trace_export_reaches_the_configured_receiver() {
        let objects: Vec<serde_json::Value> =
            serde_yaml_ng::Deserializer::from_str(include_str!("../../k8s/observability.yaml"))
                .map(|document| serde_json::Value::deserialize(document).unwrap())
                .collect();
        let endpoint = url::Url::parse(TRACE_ENDPOINT).unwrap();
        let service = objects
            .iter()
            .find(|object| {
                object["kind"] == "Service"
                    && endpoint.host_str()
                        == Some(
                            format!(
                                "{}.{}.svc.cluster.local",
                                object["metadata"]["name"].as_str().unwrap(),
                                object["metadata"]["namespace"].as_str().unwrap(),
                            )
                            .as_str(),
                        )
            })
            .unwrap();
        let port = service["spec"]["ports"]
            .as_array()
            .unwrap()
            .iter()
            .find(|port| port["port"].as_u64() == endpoint.port().map(u64::from))
            .unwrap();
        let deployment = objects
            .iter()
            .find(|object| {
                object["kind"] == "Deployment"
                    && object["spec"]["template"]["metadata"]["labels"]["app"]
                        == service["spec"]["selector"]["app"]
            })
            .unwrap();
        let container = &deployment["spec"]["template"]["spec"]["containers"][0];
        let receiver_port = container["ports"]
            .as_array()
            .unwrap()
            .iter()
            .find(|candidate| candidate["name"] == port["targetPort"])
            .unwrap()["containerPort"]
            .as_u64()
            .unwrap();
        let config_map = objects
            .iter()
            .find(|object| {
                object["kind"] == "ConfigMap"
                    && object["metadata"]["name"] == deployment["metadata"]["name"]
            })
            .unwrap();
        let config: serde_json::Value =
            serde_yaml_ng::from_str(config_map["data"]["jaeger.yaml"].as_str().unwrap()).unwrap();
        assert_eq!(
            config["receivers"]["otlp"]["protocols"]["grpc"]["endpoint"],
            format!("0.0.0.0:{receiver_port}")
        );
    }

    #[test]
    fn dashboard_uses_pinned_read_only_perses() {
        let objects: Vec<serde_json::Value> =
            serde_yaml_ng::Deserializer::from_str(include_str!("../../k8s/observability.yaml"))
                .map(|document| serde_json::Value::deserialize(document).unwrap())
                .collect();
        let deployment = objects
            .iter()
            .find(|object| object["kind"] == "Deployment" && object["metadata"]["name"] == "perses")
            .unwrap();
        let image = deployment["spec"]["template"]["spec"]["containers"][0]["image"]
            .as_str()
            .unwrap();
        assert_eq!(
            image,
            "persesdev/perses:v0.55.1@sha256:292605f009770f07aa17217e70f877fabbc74858d0af9054d395064e1c411c06"
        );
        assert!(
            objects.iter().all(|object| {
                object["kind"] != "Deployment" || object["metadata"]["name"] != "grafana"
            }),
            "the Apache-2.0 dashboard layer must not deploy Grafana"
        );
        let config_map = objects
            .iter()
            .find(|object| object["kind"] == "ConfigMap" && object["metadata"]["name"] == "perses")
            .unwrap();
        let config: serde_json::Value =
            serde_yaml_ng::from_str(config_map["data"]["config.yaml"].as_str().unwrap()).unwrap();
        assert_eq!(config["security"]["readonly"], true);
        assert_eq!(
            config["provisioning"]["folders"][0],
            "/etc/lampyr-perses/provisioning/"
        );
        assert_eq!(
            config["datasource"]["proxy"]["allowed_hosts"][0],
            "prometheus.lampyr-observability.svc.cluster.local"
        );

        let dashboard: serde_json::Value =
            serde_json::from_str(include_str!("../../k8s/observability-dashboard.json")).unwrap();
        assert_eq!(dashboard["kind"], "Dashboard");
        assert_eq!(dashboard["metadata"]["project"], "lampyr");
        assert_eq!(dashboard["metadata"]["name"], "lampyr-platform");
        let serialized = serde_json::to_string(&dashboard).unwrap();
        assert!(!serialized.contains("migration_from_grafana_not_supported"));
        assert!(serialized.contains("lampyr_router_requests_in_flight"));
        assert!(serialized.contains("http://localhost:16686/search"));
        for variable in dashboard["spec"]["variables"].as_array().unwrap() {
            assert_eq!(variable["spec"]["defaultValue"], "$__all", "{variable}");
            let matchers = variable["spec"]["plugin"]["spec"]["matchers"]
                .as_array()
                .unwrap();
            assert!(
                matchers.iter().all(|matcher| matcher
                    .as_str()
                    .unwrap()
                    .contains("lampyr_router_requests_in_flight")),
                "{variable}"
            );
        }
    }

    fn router(env: serde_json::Value, owner: Option<&str>) -> serde_json::Value {
        let mut value = json!({
            "metadata":{"annotations":{}},
            "spec":{"template":{"spec":{"containers":[{"name":"router","env":env}]}}}
        });
        if let Some(owner) = owner {
            value["metadata"]["annotations"][TRACE_OWNER] = owner.into();
        }
        value
    }

    #[test]
    fn trace_environment_is_empty_or_owned_exactly() {
        validate_router_tracing(&router(json!([]), None), None).unwrap();
        let ours = router(
            json!([
                {"name":"OTEL_EXPORTER_OTLP_ENDPOINT","value":TRACE_ENDPOINT},
                {"name":"OTEL_EXPORTER_OTLP_INSECURE","value":"true"}
            ]),
            Some(TRACE_ENDPOINT),
        );
        validate_router_tracing(&ours, Some(TRACE_ENDPOINT)).unwrap();
        for value in [
            router(
                json!([{"name":"OTEL_EXPORTER_OTLP_ENDPOINT","value":"https://external"}]),
                None,
            ),
            router(
                json!([{"name":"OTEL_EXPORTER_OTLP_ENDPOINT","valueFrom":{"secretKeyRef":{"name":"otel","key":"endpoint"}}}]),
                None,
            ),
            router(
                json!([{"name":"OTEL_EXPORTER_OTLP_TRACES_ENDPOINT","value":"https://external"}]),
                None,
            ),
            router(json!([{"name":"OTEL_SDK_DISABLED","value":"true"}]), None),
        ] {
            assert!(validate_router_tracing(&value, None).is_err(), "{value}");
        }
    }

    #[test]
    fn trace_environment_from_external_sources_is_not_claimed() {
        for source in [
            json!({"configMapRef":{"name":"otel"}}),
            json!({"secretRef":{"name":"otel"},"prefix":"OTEL_"}),
        ] {
            for owner in [None, Some(TRACE_ENDPOINT)] {
                let mut value = router(json!([]), owner);
                value["spec"]["template"]["spec"]["containers"][0]["envFrom"] = json!([source]);
                assert!(validate_router_tracing(&value, owner).is_err(), "{value}");
            }
        }
    }
}
