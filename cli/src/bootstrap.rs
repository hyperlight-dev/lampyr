// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::{Datelike, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::kube::{Kube, SYSTEM};

const SERVING: &str = "https://github.com/knative/serving/releases/download/knative-v1.23.0";
const KOURIER: &str =
    "https://github.com/knative/net-kourier/releases/download/knative-v1.23.0/kourier.yaml";
const CERT_MANAGER: &str =
    "https://github.com/cert-manager/cert-manager/releases/download/v1.21.3/cert-manager.yaml";
/// fiberd-k8s-controller labels every grant Pod it creates with this.
const GRANT_POD_SELECTOR: &str = "app.kubernetes.io/managed-by=fiberd-k8s-controller";
const CONTROLLER_NAMESPACE: &str = "fiberd-system";
/// Where k8s/cert-manager-csi-driver.yaml runs.
const CSI_DRIVER_NAMESPACE: &str = "cert-manager";
const STOCK_SERVING_DEPLOYMENTS: &[&str] = &["controller", "activator", "autoscaler"];
const SERVING_RESOURCES_FOR_CLEANUP: &[&str] = &[
    "domainmappings.serving.knative.dev",
    "services.serving.knative.dev",
    "routes.serving.knative.dev",
    "configurations.serving.knative.dev",
    "revisions.serving.knative.dev",
    "ingresses.networking.internal.knative.dev",
];

pub struct Options<'a> {
    pub root: &'a Path,
    pub name: &'a str,
    pub provider: &'a str,
    pub installation_id: &'a str,
    pub domain: &'a str,
    pub https_port: u16,
    pub fiberd_image: &'a str,
    pub router_image: &'a str,
    /// fiberd-k8s-controller publishes no release yet, so its install
    /// manifest, the stock fiberd image grant Pods run and Lampyr's helper
    /// image are installation inputs.
    pub controller_manifest: &'a str,
    pub grant_image: &'a str,
    pub helper_image: &'a str,
    /// The one repository ly deploy pushes fiberd templates to, recorded
    /// for it as templateRepository.
    pub template_repository: &'a str,
    /// The repositories templates may be pulled from, spelled as grant Pods
    /// reach them, after any registry mirror: the controller admits no
    /// registry zygote from anywhere else.
    pub zygote_registries: &'a [String],
    pub registry_mirrors: BTreeMap<String, Value>,
    pub ingress_ports: Option<Value>,
}

pub(crate) fn run(kube: &Kube<'_>, namespace: Option<&str>, args: &[&str]) -> Result<()> {
    kube.host.run(kube.command(namespace)?.args(args))
}

pub(crate) fn apply_url(kube: &Kube<'_>, url: &str) -> Result<()> {
    run(kube, None, &["apply", "--filename", url])
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CoreInventory {
    schema: u32,
    installation_id: String,
    items: Vec<Value>,
}

pub(crate) const CORE_INVENTORY_KEY: &str = "coreManifestInventory";
const CORE_INVENTORY_LIMIT: usize = 384 * 1024;

/// The SHA-256 of downloaded manifests the release publishes no checksum
/// for, so a replaced release asset is refused rather than applied.
const PINNED_MANIFESTS: &[(&str, &str)] = &[(
    CERT_MANAGER,
    "c04a51295adda2b1a775146751f973e787190778c7f35180861d9678876e5c74",
)];

fn manifest_source(kube: &Kube<'_>, source: &str) -> Result<String> {
    if source.starts_with("https://") {
        let text = crate::platform::download_text(source, 8 * 1024 * 1024, kube.host)
            .context("download platform manifest")?;
        check_pinned_manifest(source, &text)?;
        Ok(text)
    } else {
        std::fs::read_to_string(source).with_context(|| format!("read {source}"))
    }
}

fn check_pinned_manifest(source: &str, text: &str) -> Result<()> {
    use sha2::Digest as _;
    if let Some((_, expected)) = PINNED_MANIFESTS.iter().find(|(url, _)| *url == source) {
        let digest: String = sha2::Sha256::digest(text.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        ensure!(
            digest == *expected,
            "{source} has SHA-256 {digest}, not the pinned {expected}"
        );
    }
    Ok(())
}

fn manifest_items(source: &str) -> Result<Vec<Value>> {
    let mut items = Vec::new();
    for document in serde_yaml_ng::Deserializer::from_str(source) {
        let document = Value::deserialize(document)?;
        if document["kind"] == "List" {
            items.extend(
                document["items"]
                    .as_array()
                    .context("manifest List items are missing")?
                    .iter()
                    .cloned(),
            );
        } else if !document.is_null() {
            items.push(document);
        }
    }
    Ok(items)
}

fn record_manifest(value: &Value, inventory: &mut Vec<Value>) -> Result<()> {
    for item in if value["kind"] == "List" {
        value["items"]
            .as_array()
            .context("manifest List items are missing")?
            .clone()
    } else {
        vec![value.clone()]
    } {
        let api_version = item["apiVersion"]
            .as_str()
            .context("manifest apiVersion is missing")?;
        let kind = item["kind"].as_str().context("manifest kind is missing")?;
        let name = item["metadata"]["name"]
            .as_str()
            .context("manifest name is missing")?;
        let mut metadata = json!({"name":name});
        if let Some(namespace) = item["metadata"]["namespace"].as_str() {
            metadata["namespace"] = namespace.into();
        }
        let identity = json!({
            "apiVersion":api_version,
            "kind":kind,
            "metadata":metadata,
        });
        if !inventory.contains(&identity) {
            inventory.push(identity);
        }
    }
    Ok(())
}

fn apply_recorded_source(
    kube: &Kube<'_>,
    installation_id: &str,
    source: &str,
    inventory: &mut Vec<Value>,
) -> Result<()> {
    apply_recorded_source_with(kube, installation_id, source, inventory, &[])
}

fn apply_recorded_source_with(
    kube: &Kube<'_>,
    installation_id: &str,
    source: &str,
    inventory: &mut Vec<Value>,
    flags: &[&str],
) -> Result<()> {
    let source = manifest_source(kube, source)?;
    let items = manifest_items(&source)?;
    for item in &items {
        record_manifest(item, inventory)?;
    }
    persist_core_inventory(kube, installation_id, inventory.clone())?;
    let mut file = tempfile::NamedTempFile::new()?;
    std::io::Write::write_all(file.as_file_mut(), source.as_bytes())?;
    let path = file
        .path()
        .to_str()
        .context("temporary manifest path is invalid")?;
    let mut args = vec!["apply", "--filename", path];
    args.extend_from_slice(flags);
    run(kube, None, &args)?;
    Ok(())
}

fn minimal_serving_core(source: &str) -> Result<Value> {
    let items = manifest_items(source)?;
    let disabled: BTreeSet<String> = items
        .iter()
        .filter(|item| {
            item["kind"] == "Deployment"
                && item["metadata"]["namespace"] == "knative-serving"
                && matches!(
                    item["metadata"]["name"].as_str(),
                    Some(name) if STOCK_SERVING_DEPLOYMENTS.contains(&name)
                )
        })
        .filter_map(|item| item["metadata"]["name"].as_str().map(str::to_owned))
        .collect();
    ensure!(
        disabled
            == BTreeSet::from([
                "activator".to_owned(),
                "autoscaler".to_owned(),
                "controller".to_owned(),
            ]),
        "Knative Serving core is missing stock workloads"
    );
    let items: Vec<_> = items
        .into_iter()
        .filter(|item| !stock_serving_resource(item))
        .collect();
    Ok(json!({"apiVersion":"v1","kind":"List","items":items}))
}

fn stock_serving_resource(item: &Value) -> bool {
    let kind = item["kind"].as_str().unwrap_or_default();
    let name = item["metadata"]["name"].as_str().unwrap_or_default();
    matches!(
        (kind, name),
        ("Deployment", "controller" | "activator" | "autoscaler")
            | ("HorizontalPodAutoscaler", "activator")
            | ("PodDisruptionBudget", "activator-pdb")
            | ("Service", "controller" | "activator-service" | "autoscaler")
            | ("ServiceAccount", "activator")
            | ("Role", "knative-serving-activator")
            | ("RoleBinding", "knative-serving-activator")
            | ("ClusterRole", "knative-serving-activator-cluster")
            | ("ClusterRoleBinding", "knative-serving-activator-cluster")
    )
}

fn apply_recorded_manifest(
    kube: &Kube<'_>,
    installation_id: &str,
    manifest: &Value,
    inventory: &mut Vec<Value>,
) -> Result<()> {
    record_manifest(manifest, inventory)?;
    persist_core_inventory(kube, installation_id, inventory.clone())?;
    kube.apply(manifest)
}

fn fiber_serving_defaults() -> Value {
    json!({"data":{
        "revision-response-start-timeout-seconds":"0",
        "revision-idle-timeout-seconds":"0"
    }})
}

fn has_ready_endpoint(endpoints: &Value) -> bool {
    endpoints["subsets"].as_array().is_some_and(|subsets| {
        subsets.iter().any(|subset| {
            subset["addresses"]
                .as_array()
                .is_some_and(|addresses| !addresses.is_empty())
        })
    })
}

fn wait_for_webhook_endpoint(kube: &Kube<'_>) -> Result<()> {
    kube.wait(
        "Knative webhook endpoint readiness",
        Duration::from_secs(30),
        || {
            let Some(endpoints) = kube.optional("knative-serving", "endpoints", "webhook")? else {
                return Ok(None);
            };
            Ok(has_ready_endpoint(&endpoints).then_some(()))
        },
    )
}

fn webhook_is_starting(error: &anyhow::Error) -> bool {
    let message = format!("{error:#}");
    message.contains("failed calling webhook")
        && (message.contains("connection refused")
            || message.contains("no endpoints available")
            || message.contains("context deadline exceeded"))
}

/// Whether cert-manager's webhook refused `error` only because it is
/// starting. Until cainjector writes the webhook's CA bundle, the API
/// server cannot verify the certificate it serves, so a fresh install
/// fails x509 verification for a while too.
fn cert_manager_webhook_is_starting(error: &anyhow::Error) -> bool {
    let message = format!("{error:#}");
    webhook_is_starting(error)
        || (message.contains("failed calling webhook \"webhook.cert-manager.io\"")
            && message.contains("x509: certificate signed by unknown authority"))
}

fn patch_fiber_serving_defaults(kube: &Kube<'_>) -> Result<()> {
    kube.wait(
        "Knative webhook configuration readiness",
        Duration::from_secs(60),
        || match kube.patch(
            "knative-serving",
            "configmap",
            "config-defaults",
            &fiber_serving_defaults(),
        ) {
            Ok(()) => Ok(Some(())),
            Err(error) if webhook_is_starting(&error) => Ok(None),
            Err(error) => Err(error),
        },
    )
}

fn encode_core_inventory(installation_id: &str, items: Vec<Value>) -> Result<String> {
    let encoded = serde_json::to_string(&CoreInventory {
        schema: 1,
        installation_id: installation_id.to_owned(),
        items,
    })?;
    ensure!(
        encoded.len() <= CORE_INVENTORY_LIMIT,
        "core platform manifest inventory exceeds 384 KiB"
    );
    Ok(encoded)
}

fn decode_core_inventory(encoded: &str, installation_id: &str) -> Result<Vec<Value>> {
    ensure!(
        encoded.len() <= CORE_INVENTORY_LIMIT,
        "core platform manifest inventory exceeds 384 KiB"
    );
    let inventory: CoreInventory =
        serde_json::from_str(encoded).context("decode core platform manifest inventory")?;
    ensure!(inventory.schema == 1, "unsupported core manifest inventory");
    ensure!(
        inventory.installation_id == installation_id,
        "core platform manifest inventory belongs to another installation"
    );
    for item in &inventory.items {
        crate::cluster::validate_manifest(item)?;
    }
    Ok(inventory.items)
}

fn persist_core_inventory(kube: &Kube<'_>, installation_id: &str, items: Vec<Value>) -> Result<()> {
    ensure!(
        !items.is_empty(),
        "core platform manifest inventory is empty"
    );
    let installed = kube.installation()?;
    ensure!(
        installed.required("installationId")? == installation_id,
        "cluster is not owned by this installation"
    );
    let secret: Value = kube.get("kube-system", "secret", crate::cluster::RUNTIME_SECRET)?;
    ensure!(
        secret["metadata"]["labels"]["lampyr.dev/installation-id"] == installation_id,
        "runtime inventory belongs to another installation"
    );
    let encoded = encode_core_inventory(installation_id, items)?;
    kube.patch(
        "kube-system",
        "secret",
        crate::cluster::RUNTIME_SECRET,
        &json!({"data":{CORE_INVENTORY_KEY:STANDARD.encode(encoded)}}),
    )?;
    Ok(())
}

fn existing_core_inventory(kube: &Kube<'_>, installation_id: &str) -> Result<Option<Vec<Value>>> {
    let installed = kube.installation()?;
    ensure!(
        installed.required("installationId")? == installation_id,
        "cluster is not owned by this installation"
    );
    let Some(secret) = kube.optional("kube-system", "secret", crate::cluster::RUNTIME_SECRET)?
    else {
        return Ok(None);
    };
    ensure!(
        secret["metadata"]["labels"]["lampyr.dev/installation-id"] == installation_id,
        "runtime inventory belongs to another installation"
    );
    let Some(encoded) = secret["data"][CORE_INVENTORY_KEY].as_str() else {
        return Ok(None);
    };
    let encoded = STANDARD
        .decode(encoded)
        .context("decode core platform manifest inventory data")?;
    let encoded =
        String::from_utf8(encoded).context("core platform manifest inventory is not UTF-8")?;
    decode_core_inventory(&encoded, installation_id).map(Some)
}

fn read_core_inventory(kube: &Kube<'_>, installation_id: &str) -> Result<Vec<Value>> {
    existing_core_inventory(kube, installation_id)?.context(
        "core platform manifest inventory is missing from the cluster; reinstall the platform before uninstalling",
    )
}

pub fn install(kube: &Kube<'_>, options: &Options<'_>) -> Result<()> {
    ensure!(
        !options.installation_id.is_empty(),
        "installation identity is required"
    );
    ensure!(
        !options.fiberd_image.is_empty() && !options.router_image.is_empty(),
        "platform runtime images are required"
    );
    ensure!(
        !options.controller_manifest.is_empty()
            && !options.grant_image.is_empty()
            && !options.helper_image.is_empty()
            && !options.template_repository.is_empty()
            && !options.zygote_registries.is_empty(),
        "controllerManifest, grantImage, helperImage, templateRepository and zygoteRegistries are required"
    );
    ensure!(options.https_port > 0, "HTTPS port must be positive");
    crate::project::validate_name(options.name)?;
    claim(
        kube,
        options.name,
        options.provider,
        options.installation_id,
    )?;
    let mut inventory = existing_core_inventory(kube, options.installation_id)?.unwrap_or_default();
    delete_retired(kube, options.installation_id, &mut inventory)?;
    install_platform(kube, options, &mut inventory)?;
    single_node_pdbs(kube)?;
    persist_core_inventory(kube, options.installation_id, inventory)
}

/// The objects of Lampyr's grant issuer (k8s/grant-controller.yaml before
/// fiberd-k8s-controller replaced it) that no current manifest applies.
/// Its Pod would keep issuing Pods for the controller's grants, so a
/// reinstall deletes them. The ServiceAccounts fiberd-grant and
/// fiber-router, the lampyr-router-grants Role and the lampyr-grant-homes
/// NetworkPolicy are applied again under the same names, so they stay.
///
/// Explicit, rather than deleting whatever the new install no longer
/// records: an install records only what it applies, and it applies
/// cert-manager and its csi-driver only when the cluster lacks them, so a
/// reinstall would delete the ones the first install made.
const RETIRED: &[(&str, &str, Option<&str>, &str)] = &[
    ("apps/v1", "Deployment", Some(SYSTEM), "grant-issuer"),
    ("v1", "Service", Some(SYSTEM), "grant-issuer"),
    ("v1", "ServiceAccount", Some(SYSTEM), "grant-issuer"),
    (RBAC, "ClusterRoleBinding", None, "lampyr-grant-issuer"),
    (RBAC, "ClusterRole", None, "lampyr-grant-issuer"),
    (RBAC, "ClusterRoleBinding", None, "lampyr-grant-namespace"),
    (RBAC, "ClusterRole", None, "lampyr-grant-namespace"),
    (RBAC, "RoleBinding", Some("default"), "lampyr-grant-home"),
    (RBAC, "Role", Some("default"), "lampyr-grant-home"),
];
const RBAC: &str = "rbac.authorization.k8s.io/v1";

/// Removes from `inventory` the retired objects it records, and returns
/// them. Only those are deleted, so an object of the same name Lampyr
/// never made is left alone.
fn take_retired(inventory: &mut Vec<Value>) -> Vec<Value> {
    let mut retired = Vec::new();
    for (api_version, kind, namespace, name) in RETIRED {
        let mut metadata = json!({ "name": name });
        if let Some(namespace) = namespace {
            metadata["namespace"] = (*namespace).into();
        }
        let identity = json!({ "apiVersion": api_version, "kind": kind, "metadata": metadata });
        if let Some(index) = inventory.iter().position(|item| *item == identity) {
            retired.push(inventory.remove(index));
        }
    }
    retired
}

fn delete_retired(
    kube: &Kube<'_>,
    installation_id: &str,
    inventory: &mut Vec<Value>,
) -> Result<()> {
    // Only a reinstall finds an inventory, and only it can find the
    // issuer's home Pods.
    let reinstall = !inventory.is_empty();
    let retired = take_retired(inventory);
    if !retired.is_empty() {
        delete_retired_objects(kube, installation_id, inventory, retired)?;
    }
    if reinstall {
        delete_retired_homes(kube)?;
    }
    Ok(())
}

/// The grant homes the retired issuer made: the labels the old serving
/// controller gave their CapacityGrants' Pods, and the grant label the
/// issuer added. fiberd-k8s-controller's grant Pods carry those too, so
/// only Pods without its managed-by and grant-uid labels are the issuer's.
const RETIRED_HOME_SELECTOR: &str = "app=fiberd,lampyr.dev/home=true,fiberd.io/grant,\
     app.kubernetes.io/managed-by!=fiberd-k8s-controller,!fiberd.io/grant-uid";

/// Deletes the retired issuer's grant homes, which nothing deletes once
/// the issuer has gone. They hold hyperlight.dev/hypervisor, so on a small
/// cluster the controller's grant Pods could never schedule beside them.
fn delete_retired_homes(kube: &Kube<'_>) -> Result<()> {
    kube.host.run(
        kube.command(Some("default"))?
            .args(["delete", "pods", "--selector", RETIRED_HOME_SELECTOR])
            .args(["--ignore-not-found=true", "--wait=true", "--timeout=180s"]),
    )
}

fn delete_retired_objects(
    kube: &Kube<'_>,
    installation_id: &str,
    inventory: &[Value],
    retired: Vec<Value>,
) -> Result<()> {
    let mut file = tempfile::NamedTempFile::new()?;
    serde_json::to_writer(
        file.as_file_mut(),
        &json!({ "apiVersion": "v1", "kind": "List", "items": retired }),
    )?;
    // Foreground, so the issuer's Pod is gone before the controller that
    // replaces it starts.
    kube.host.run(
        kube.command(None)?
            .args(["delete", "--filename"])
            .arg(file.path())
            .args([
                "--ignore-not-found=true",
                "--cascade=foreground",
                "--wait=true",
                "--timeout=180s",
            ]),
    )?;
    if !inventory.is_empty() {
        persist_core_inventory(kube, installation_id, inventory.to_vec())?;
    }
    Ok(())
}

pub fn claim(kube: &Kube<'_>, name: &str, provider: &str, installation_id: &str) -> Result<()> {
    let previous = kube.optional("kube-system", "configmap", "lampyr-install")?;
    if let Some(previous) = &previous {
        ensure!(
            previous
                .pointer("/data/installationId")
                .and_then(Value::as_str)
                == Some(installation_id),
            "cluster already belongs to another Lampyr installation"
        );
    } else {
        ensure!(
            kube.optional("default", "namespace", "knative-serving")?
                .is_none(),
            "cluster already has Knative and is not owned by this installation"
        );
    }
    if previous.is_none() {
        let mut metadata = json!({
            "apiVersion": "v1", "kind": "ConfigMap",
            "metadata": { "name": "lampyr-install", "namespace": "kube-system" },
            "data": { "installationId": installation_id, "cluster": name }
        });
        if !provider.is_empty() {
            metadata["data"]["provider"] = provider.into();
        }
        kube.apply(&metadata)?;
    }
    Ok(())
}

fn install_platform(
    kube: &Kube<'_>,
    options: &Options<'_>,
    inventory: &mut Vec<Value>,
) -> Result<()> {
    kube.host
        .step("Configuring your cluster (Knative Serving)", || {
            run(
                kube,
                None,
                &[
                    "wait",
                    "--for=condition=Ready",
                    "node",
                    "--all",
                    "--timeout=120s",
                ],
            )?;
            apply_recorded_source(
                kube,
                options.installation_id,
                &format!("{SERVING}/serving-crds.yaml"),
                inventory,
            )?;
            kube.wait_crds(
                &["images.caching.internal.knative.dev"],
                Duration::from_secs(180),
            )?;
            if kube
                .optional("default", "namespace", "knative-serving")?
                .is_some()
            {
                for deployment in STOCK_SERVING_DEPLOYMENTS {
                    ensure!(
                        kube.optional("knative-serving", "deployment", deployment)?
                            .is_none(),
                        "cluster contains legacy Knative deployment/{deployment}; rebuild the cluster before installing the Fiber control plane"
                    );
                }
            }
            let source = manifest_source(kube, &format!("{SERVING}/serving-core.yaml"))?;
            let core = minimal_serving_core(&source)?;
            apply_recorded_manifest(kube, options.installation_id, &core, inventory)?;
            rollout(kube, "knative-serving", "deployment", "webhook", "180s")?;
            wait_for_webhook_endpoint(kube)?;
            patch_fiber_serving_defaults(kube)
        })?;
    kube.host
        .step("Wiring your network and HTTPS ingress", || {
            apply_recorded_source(kube, options.installation_id, KOURIER, inventory)?;
            rollout(
                kube,
                "knative-serving",
                "deployment",
                "net-kourier-controller",
                "180s",
            )?;
            rollout(
                kube,
                "kourier-system",
                "deployment",
                "3scale-kourier-gateway",
                "180s",
            )?;
            if let Some(ports) = &options.ingress_ports {
                ensure!(ports.is_array(), "ingress ports must be an array");
                kube.patch(
                    "kourier-system",
                    "service",
                    "kourier",
                    &json!({ "spec": { "ports": ports } }),
                )?;
            } else {
                kube.wait("public ingress address", Duration::from_secs(600), || {
                    let service: Value = kube.get("kourier-system", "service", "kourier")?;
                    Ok(service
                        .pointer("/status/loadBalancer/ingress")
                        .and_then(Value::as_array)
                        .filter(|addresses| !addresses.is_empty())
                        .map(|_| ()))
                })?;
            }
            tls(kube, options.domain)?;
            kube.patch(
                "knative-serving",
                "configmap",
                "config-kourier",
                &json!({ "data": {
            "certs-secret-name": "lampyr-kourier-tls", "certs-secret-namespace": "kourier-system"
        } }),
            )?;
            kube.patch(
                "knative-serving",
                "configmap",
                "config-network",
                &json!({ "data": { "ingress-class": "kourier.ingress.networking.knative.dev" } }),
            )?;
            run(
                kube,
                Some("knative-serving"),
                &[
                    "patch",
                    "configmap",
                    "config-domain",
                    "--type",
                    "json",
                    "--patch",
                    &serde_json::to_string(&json!([
                        { "op": "replace", "path": "/data", "value": { options.domain: "" } }
                    ]))?,
                ],
            )
        })?;
    kube.host.step("Making KVM available to Grant homes", || {
        apply_recorded_source(
            kube,
            options.installation_id,
            options
                .root
                .join("k8s/hyperlight-device-plugin.yaml")
                .to_str()
                .context("platform path must be UTF-8")?,
            inventory,
        )?;
        rollout(
            kube,
            "hyperlight-system",
            "daemonset",
            "hyperlight-device-plugin",
            "180s",
        )?;
        kvm_capacity(kube, Duration::from_secs(120))
    })?;
    kube.host.step("Making things fast (fiberd-k8s-controller and protocol consumer)", || {
        kube.apply(&json!({
            "apiVersion": "v1", "kind": "Namespace",
            "metadata": { "name": SYSTEM, "labels": { "app.kubernetes.io/managed-by": "lampyr" } }
        }))?;
        install_controller(kube, options, inventory)?;
        let router = deployment_manifest(
            &options.root.join("k8s/fiber-router.yaml"),
            "fiber-router",
            "router",
            options.router_image,
        )?;
        record_manifest(&router, inventory)?;
        persist_core_inventory(kube, options.installation_id, inventory.clone())?;
        kube.apply(&router)?;
        rollout(kube, SYSTEM, "deployment", "fiber-router", "180s")?;
        let mut serving = deployment_manifest(
            &options.root.join("k8s/serving-controller.yaml"),
            "lampyr-serving-controller",
            "controller",
            options.fiberd_image,
        )?;
        if !options.registry_mirrors.is_empty() {
            let container = find_container(&mut serving, "lampyr-serving-controller", "controller")?;
            let env = container.as_object_mut().context("serving controller container must be a mapping")?
                .entry("env").or_insert_with(|| json!([])).as_array_mut()
                .context("serving controller environment must be an array")?;
            env.retain(|entry| entry["name"] != "FIBERD_REGISTRY_MIRRORS");
            env.push(json!({ "name": "FIBERD_REGISTRY_MIRRORS", "value": serde_json::to_string(&options.registry_mirrors)? }));
        }
        record_manifest(&serving, inventory)?;
        persist_core_inventory(kube, options.installation_id, inventory.clone())?;
        kube.apply(&serving)?;
        rollout(
            kube,
            "knative-serving",
            "deployment",
            "lampyr-serving-controller",
            "180s",
        )?;
        Ok(())
    })?;
    kube.patch("kube-system", "configmap", "lampyr-install", &json!({ "data": {
        "version": env!("CARGO_PKG_VERSION"), "domain": options.domain, "httpsPort": options.https_port.to_string(),
        "fiberdImage": options.fiberd_image, "routerImage": options.router_image,
        "grantImage": options.grant_image, "helperImage": options.helper_image,
        "templateRepository": options.template_repository
    } }))?;
    Ok(())
}

/// Installs fiberd-k8s-controller and makes namespace default its tenant:
/// cert-manager and its csi-driver unless the cluster has them, the
/// controller, the GrantPolicy that admits Lampyr's grant Pods, and the
/// tenant objects the controller's config/tenant would add but its author
/// RoleBinding: Lampyr's serving controller writes every CapacityGrant in
/// default under its own ClusterRole, so no group of people needs to.
fn install_controller(
    kube: &Kube<'_>,
    options: &Options<'_>,
    inventory: &mut Vec<Value>,
) -> Result<()> {
    if kube
        .optional("default", "crd", "certificates.cert-manager.io")?
        .is_none()
    {
        apply_recorded_source(kube, options.installation_id, CERT_MANAGER, inventory)?;
        for deployment in [
            "cert-manager",
            "cert-manager-cainjector",
            "cert-manager-webhook",
        ] {
            rollout(kube, "cert-manager", "deployment", deployment, "300s")?;
        }
    } else {
        // A cert-manager Lampyr didn't install may run in any namespace
        // under any name, so only its API is waited for here; the
        // controller manifest's apply below retries until its webhook
        // admits the CA Certificate.
        kube.wait_crds(
            &[
                "certificates.cert-manager.io",
                "issuers.cert-manager.io",
                "clusterissuers.cert-manager.io",
            ],
            Duration::from_secs(120),
        )?;
    }
    if kube
        .optional("default", "csidriver", "csi.cert-manager.io")?
        .is_none()
    {
        // The csi-driver manifest runs in cert-manager, which it doesn't
        // create: a cert-manager Lampyr didn't install may run elsewhere.
        if kube
            .optional("default", "namespace", CSI_DRIVER_NAMESPACE)?
            .is_none()
        {
            apply_recorded_manifest(
                kube,
                options.installation_id,
                &json!({
                    "apiVersion": "v1", "kind": "Namespace",
                    "metadata": { "name": CSI_DRIVER_NAMESPACE }
                }),
                inventory,
            )?;
        }
        let csi_driver = options.root.join("k8s/cert-manager-csi-driver.yaml");
        apply_recorded_source(
            kube,
            options.installation_id,
            csi_driver.to_str().context("platform path must be UTF-8")?,
            inventory,
        )?;
        rollout(
            kube,
            CSI_DRIVER_NAMESPACE,
            "daemonset",
            "cert-manager-csi-driver",
            "180s",
        )?;
    }
    // cert-manager reports Available before its webhook admits the
    // controller's CA Certificate.
    kube.wait(
        "fiberd-k8s-controller manifest admission",
        Duration::from_secs(120),
        // The CapacityGrant CRD embeds a whole PodTemplateSpec schema, too
        // large for the annotation a client-side apply keeps.
        || match apply_recorded_source_with(
            kube,
            options.installation_id,
            options.controller_manifest,
            inventory,
            &["--server-side", "--force-conflicts"],
        ) {
            Ok(()) => Ok(Some(())),
            Err(error) if cert_manager_webhook_is_starting(&error) => Ok(None),
            Err(error) => Err(error),
        },
    )?;
    kube.wait_crds(
        &["capacitygrants.fiberd.io", "grantpolicies.fiberd.io"],
        Duration::from_secs(120),
    )?;
    rollout(
        kube,
        CONTROLLER_NAMESPACE,
        "deployment",
        "fiberd-k8s-controller",
        "180s",
    )?;
    // The policy admits the fiberd and helper images and the templates only
    // from listed repositories, each in a list of its own, and the
    // hyperlight runtime runs privileged, which it allows only in a listed
    // namespace.
    let policy = json!({
        "apiVersion": "fiberd.io/v1alpha1", "kind": "GrantPolicy",
        "metadata": { "name": "default" },
        "spec": {
            "images": [image_repository("grantImage", options.grant_image)?],
            "helperImages": [image_repository("helperImage", options.helper_image)?],
            "registries": options.zygote_registries,
            "runtimes": ["hyperlight"],
            "privilegedNamespaces": ["default"],
        }
    });
    apply_recorded_manifest(kube, options.installation_id, &policy, inventory)?;
    let tenant = options.root.join("k8s/fiberd-tenant.yaml");
    apply_recorded_source(
        kube,
        options.installation_id,
        tenant.to_str().context("platform path must be UTF-8")?,
        inventory,
    )
}

/// The repository a GrantPolicy lists for an image reference, which must
/// carry a digest because the controller's admission policy pulls by it.
pub(crate) fn image_repository(field: &str, image: &str) -> Result<String> {
    let (name, digest) = image
        .split_once('@')
        .with_context(|| format!("{field} must be pinned by digest"))?;
    // The policy pulls by digest, so a short one would refuse every grant.
    ensure!(
        crate::image::is_sha256_digest(digest),
        "{field} must be pinned by a full sha256:<64 hex> digest"
    );
    let last = name.rfind('/').map_or(0, |slash| slash + 1);
    let repository = match name[last..].find(':') {
        Some(colon) => &name[..last + colon],
        None => name,
    };
    // The API server would refuse the GrantPolicy only once the cluster
    // exists. Its images and helperImages entries take the registries
    // pattern without a scheme (ImageRepositoryPattern).
    ensure!(
        repository.len() <= MAX_IMAGE_REPOSITORY_LENGTH
            && !repository.starts_with("http://")
            && crate::platform_host::registry_pattern(repository),
        "{field} {image:?} must name a fully qualified repository, such as \
         ghcr.io/team/fiberd@sha256:..., whose registry is localhost or has a dot or a port, \
         with a lowercase path, in at most {MAX_IMAGE_REPOSITORY_LENGTH} characters"
    );
    Ok(repository.to_owned())
}

/// The GrantPolicy's limit on an images or helperImages entry.
const MAX_IMAGE_REPOSITORY_LENGTH: usize = 255;

pub fn rollout(
    kube: &Kube<'_>,
    namespace: &str,
    kind: &str,
    name: &str,
    timeout: &str,
) -> Result<()> {
    run(
        kube,
        Some(namespace),
        &[
            "rollout",
            "status",
            &format!("{kind}/{name}"),
            &format!("--timeout={timeout}"),
        ],
    )
}

pub fn deployment_manifest(
    path: &Path,
    deployment: &str,
    container: &str,
    image: &str,
) -> Result<Value> {
    let source =
        std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let items: Vec<Value> = serde_yaml_ng::Deserializer::from_str(&source)
        .map(Value::deserialize)
        .collect::<std::result::Result<_, _>>()?;
    let mut resources = json!({ "apiVersion": "v1", "kind": "List", "items": items.into_iter().filter(|item| !item.is_null()).collect::<Vec<_>>() });
    let container = find_container(&mut resources, deployment, container)?;
    container["image"] = image.into();
    container["imagePullPolicy"] = "IfNotPresent".into();
    Ok(resources)
}

pub(crate) fn find_container<'a>(
    resources: &'a mut Value,
    deployment: &str,
    container: &str,
) -> Result<&'a mut Value> {
    resources["items"]
        .as_array_mut()
        .context("manifest items are missing")?
        .iter_mut()
        .find(|resource| {
            resource["kind"] == "Deployment" && resource["metadata"]["name"] == deployment
        })
        .and_then(|resource| resource.pointer_mut("/spec/template/spec/containers"))
        .and_then(Value::as_array_mut)
        .and_then(|containers| {
            containers
                .iter_mut()
                .find(|value| value["name"] == container)
        })
        .with_context(|| {
            format!("manifest is missing deployment/{deployment} container/{container}")
        })
}

fn tls(kube: &Kube<'_>, domain: &str) -> Result<()> {
    crate::platform_host::validate_domain(domain)?;
    if let Some(secret) = kube.optional("kourier-system", "secret", "lampyr-kourier-tls")?
        && secret
            .pointer("/metadata/annotations/lampyr.dev~1domain")
            .and_then(Value::as_str)
            == Some(domain)
    {
        return Ok(());
    }
    let mut parameters =
        rcgen::CertificateParams::new(vec![format!("*.{domain}"), format!("*.default.{domain}")])?;
    parameters
        .distinguished_name
        .push(rcgen::DnType::CommonName, "Lampyr local development");
    let today = Utc::now();
    let expiry = today + chrono::TimeDelta::days(365);
    parameters.not_before =
        rcgen::date_time_ymd(today.year(), today.month() as u8, today.day() as u8);
    parameters.not_after =
        rcgen::date_time_ymd(expiry.year(), expiry.month() as u8, expiry.day() as u8);
    let key = rcgen::KeyPair::generate()?;
    let certificate = parameters.self_signed(&key)?;
    kube.apply(&json!({
        "apiVersion": "v1", "kind": "Secret", "type": "kubernetes.io/tls",
        "metadata": { "name": "lampyr-kourier-tls", "namespace": "kourier-system", "annotations": { "lampyr.dev/domain": domain } },
        "data": { "tls.crt": STANDARD.encode(certificate.pem()), "tls.key": STANDARD.encode(key.serialize_pem()) }
    }))
}

pub(crate) fn delete_manifest(kube: &Kube<'_>, source: &str) -> Result<Vec<String>> {
    let source = manifest_source(kube, source).context("load manifest for deletion")?;
    delete_inventory(kube, manifest_items(&source)?)
}

/// Reads the groups and kinds of `kubectl get crds` printed one CRD a line
/// as group, tab, kind. A line without a tab names no CRD: the API server
/// requires both, so only a stray line can lack one.
fn crd_kinds(listed: &str) -> BTreeSet<(String, String)> {
    listed
        .lines()
        .filter_map(|line| {
            let (group, kind) = line.split_once('\t')?;
            Some((group.to_owned(), kind.to_owned()))
        })
        .collect()
}

fn delete_inventory(kube: &Kube<'_>, items: Vec<Value>) -> Result<Vec<String>> {
    let discovery: Value =
        serde_json::from_str(&kube.capture(kube.command(None)?.args(["get", "--raw", "/apis"]))?)?;
    let mut versions = BTreeSet::from(["v1".to_owned()]);
    for group in discovery["groups"]
        .as_array()
        .context("API groups are missing")?
    {
        for version in group["versions"]
            .as_array()
            .context("API group versions are missing")?
        {
            versions.insert(
                version["groupVersion"]
                    .as_str()
                    .context("API group version is missing")?
                    .to_owned(),
            );
        }
    }
    let mut kinds = BTreeMap::new();
    // Only each CRD's group and kind: whole CRDs carry their schemas, and
    // a cluster with kube-prometheus-stack alone lists over 8 MiB of them.
    let crds = kube.capture(kube.command(None)?.args([
        "get",
        "crds",
        "--output",
        r#"jsonpath={range .items[*]}{.spec.group}{"\t"}{.spec.names.kind}{"\n"}{end}"#,
    ]))?;
    let custom = crd_kinds(&crds);
    let mut namespaces = Vec::new();
    let mut remaining = Vec::new();
    for item in items {
        let version = item["apiVersion"]
            .as_str()
            .context("manifest apiVersion is missing")?;
        let kind = item["kind"].as_str().context("manifest kind is missing")?;
        if kind == "Namespace" {
            namespaces.push(
                item.pointer("/metadata/name")
                    .and_then(Value::as_str)
                    .context("namespace name is missing")?
                    .to_owned(),
            );
            continue;
        }
        if !versions.contains(version) {
            continue;
        }
        if !kinds.contains_key(version) {
            let path = if version == "v1" {
                "/api/v1".to_owned()
            } else {
                format!("/apis/{version}")
            };
            let discovery: Value = serde_json::from_str(
                &kube.capture(kube.command(None)?.args(["get", "--raw", &path]))?,
            )?;
            let available: BTreeSet<String> = discovery["resources"]
                .as_array()
                .context("API resource discovery is missing")?
                .iter()
                .filter_map(|resource| resource["kind"].as_str().map(str::to_owned))
                .collect();
            kinds.insert(version.to_owned(), available);
        }
        if kinds[version].contains(kind) {
            let group = version
                .split('/')
                .next()
                .context("manifest API group is missing")?;
            let is_custom = custom.contains(&(group.to_owned(), kind.to_owned()));
            remaining.push((item, is_custom));
        }
    }
    for (pass, items) in deletion_passes(&remaining).into_iter().enumerate() {
        if items.is_empty() {
            continue;
        }
        let mut file = tempfile::NamedTempFile::new()?;
        serde_json::to_writer(
            file.as_file_mut(),
            &json!({ "apiVersion": "v1", "kind": "List", "items": items }),
        )?;
        let delete = || {
            kube.host.run(
                kube.command(None)?
                    .args(["delete", "--filename"])
                    .arg(file.path())
                    .args([
                        "--ignore-not-found=true",
                        if pass < 2 {
                            "--wait=true"
                        } else {
                            "--wait=false"
                        },
                        "--timeout=180s",
                    ]),
            )
        };
        // The API server may enforce a deleted admission policy for a
        // moment longer, so a custom object it guarded is retried. The
        // deadline outlasts kubectl's own --timeout.
        kube.wait(
            "platform object deletion",
            Duration::from_secs(240),
            || match delete() {
                Ok(()) => Ok(Some(())),
                Err(error)
                    if pass == 1 && format!("{error:#}").contains("ValidatingAdmissionPolicy") =>
                {
                    Ok(None)
                }
                Err(error) => Err(error),
            },
        )?;
    }
    Ok(namespaces)
}

/// Orders inventory deletion. Admission policies go first, since
/// fiberd-k8s-controller's refuses deleting the GrantPolicy named default.
/// Custom objects go next, so their controllers can finalize them before the
/// controllers, CRDs and namespaces go last.
fn deletion_passes(remaining: &[(Value, bool)]) -> [Vec<&Value>; 3] {
    let mut passes: [Vec<&Value>; 3] = Default::default();
    for (item, custom) in remaining {
        let admission = matches!(
            item["kind"].as_str(),
            Some("ValidatingAdmissionPolicy" | "ValidatingAdmissionPolicyBinding")
        );
        passes[if admission {
            0
        } else if *custom {
            1
        } else {
            2
        }]
        .push(item);
    }
    passes
}

pub fn health(kube: &Kube<'_>) -> Result<()> {
    let installed = kube.installation()?;
    run(
        kube,
        None,
        &[
            "wait",
            "--for=condition=Ready",
            "node",
            "--all",
            "--timeout=10s",
        ],
    )?;
    for (namespace, kind, name) in [
        ("hyperlight-system", "daemonset", "hyperlight-device-plugin"),
        (CONTROLLER_NAMESPACE, "deployment", "fiberd-k8s-controller"),
        (SYSTEM, "deployment", "fiber-router"),
        ("knative-serving", "deployment", "lampyr-serving-controller"),
        ("knative-serving", "deployment", "webhook"),
        ("knative-serving", "deployment", "net-kourier-controller"),
        ("kourier-system", "deployment", "3scale-kourier-gateway"),
    ] {
        rollout(kube, namespace, kind, name, "10s")?;
    }
    kvm_capacity(kube, Duration::from_secs(10))?;
    let pods: Vec<Value> = kube.list("default", "pods", "lampyr.dev/home=true")?;
    for pod in pods {
        ensure!(
            pod.pointer("/status/conditions")
                .and_then(Value::as_array)
                .is_some_and(|conditions| {
                    conditions.iter().any(|condition| {
                        condition["type"] == "Ready" && condition["status"] == "True"
                    })
                }),
            "Grant home {} is not ready",
            pod["metadata"]["name"]
        );
    }

    if installed
        .data
        .get("eventing")
        .is_some_and(|value| value == "true")
    {
        crate::eventing::health(kube)?;
    }
    Ok(())
}

fn kvm_capacity(kube: &Kube<'_>, timeout: Duration) -> Result<()> {
    kube.wait("KVM capacity on Linux/amd64 nodes", timeout, || {
        let nodes: Vec<Value> = kube.list(
            "default",
            "nodes",
            "kubernetes.io/os=linux,kubernetes.io/arch=amd64",
        )?;
        let mut eligible = false;
        let mut ready = true;
        for node in nodes {
            if node.pointer("/spec/unschedulable") == Some(&json!(true))
                || node
                    .pointer("/spec/taints")
                    .and_then(Value::as_array)
                    .is_some_and(|taints| {
                        taints.iter().any(|taint| {
                            matches!(taint["effect"].as_str(), Some("NoSchedule" | "NoExecute"))
                        })
                    })
            {
                continue;
            }
            eligible = true;
            ready &= match node.pointer("/status/capacity/hyperlight.dev~1hypervisor") {
                Some(value) => {
                    positive_quantity(value.as_str().context("KVM capacity must be text")?)?
                }
                None => false,
            };
        }
        Ok((eligible && ready).then_some(()))
    })
}

fn positive_quantity(value: &str) -> Result<bool> {
    let numeric = [
        "Ki", "Mi", "Gi", "Ti", "Pi", "Ei", "n", "u", "m", "k", "M", "G", "T", "P", "E",
    ]
    .iter()
    .find_map(|suffix| value.strip_suffix(suffix))
    .unwrap_or(value);
    let quantity = numeric
        .parse::<f64>()
        .context("invalid Kubernetes resource quantity")?;
    ensure!(
        quantity.is_finite() && quantity >= 0.0,
        "invalid Kubernetes resource quantity"
    );
    Ok(quantity > 0.0)
}

pub(crate) fn single_node_pdbs(kube: &Kube<'_>) -> Result<()> {
    let nodes: Vec<Value> = kube.list("default", "nodes", "")?;
    if nodes.len() != 1 {
        return Ok(());
    }
    let budgets: Value = serde_json::from_str(&kube.capture(kube.command(None)?.args([
        "get",
        "poddisruptionbudgets",
        "--all-namespaces",
        "-o",
        "json",
    ]))?)?;
    for budget in budgets["items"]
        .as_array()
        .context("Pod disruption budgets must contain items")?
    {
        let namespace = budget["metadata"]["namespace"]
            .as_str()
            .context("Pod disruption budget namespace is missing")?;
        if matches!(
            namespace,
            "knative-serving" | "knative-eventing" | "kourier-system"
        ) {
            kube.patch(
                namespace,
                "poddisruptionbudget",
                budget["metadata"]["name"]
                    .as_str()
                    .context("Pod disruption budget name is missing")?,
                &json!({"spec":{"minAvailable":0,"maxUnavailable":null}}),
            )?;
        }
    }
    Ok(())
}
pub fn uninstall(kube: &Kube<'_>, root: &Path, installation_id: &str) -> Result<()> {
    uninstall_retaining_identity(kube, root, installation_id)?;
    remove_installation_identity(kube)
}

pub(crate) fn uninstall_retaining_identity(
    kube: &Kube<'_>,
    root: &Path,
    installation_id: &str,
) -> Result<()> {
    let installed = kube.installation()?;
    ensure!(
        installed.required("installationId")? == installation_id,
        "cluster is not owned by this installation"
    );
    if installed
        .data
        .get("eventing")
        .is_some_and(|value| value == "true")
    {
        crate::eventing::uninstall(kube, root)?;
    }
    for resource in SERVING_RESOURCES_FOR_CLEANUP {
        if kube.optional("default", "crd", resource)?.is_some() {
            run(
                kube,
                None,
                &[
                    "delete",
                    resource,
                    "--all",
                    "--all-namespaces",
                    "--ignore-not-found=true",
                    "--wait=true",
                    "--timeout=120s",
                ],
            )?;
        }
    }
    if kube
        .optional("default", "crd", "capacitygrants.fiberd.io")?
        .is_some()
    {
        run(
            kube,
            Some("default"),
            &[
                "delete",
                "capacitygrants.fiberd.io",
                "--all",
                "--cascade=foreground",
                "--wait=true",
                "--timeout=120s",
            ],
        )?;
    }
    // Grant Pods mount certificate volumes from cert-manager's csi-driver,
    // which the inventory deletes too, and a Pod whose driver has gone can
    // never unmount its volume, so every such Pod must be gone before the
    // driver. They go by garbage collection once their Revisions do, so
    // wait for them. The router's certificate is a Secret, which needs no
    // driver.
    kube.wait("grant Pod deletion", Duration::from_secs(180), || {
        let pods: Vec<Value> = kube.list("default", "pods", GRANT_POD_SELECTOR)?;
        Ok(pods.is_empty().then_some(()))
    })?;
    // The fleet key opens only templates of the services just deleted, and
    // a key left behind would be silently reused by the next install. Its
    // public half goes with the SYSTEM namespace.
    run(
        kube,
        Some(crate::fiberd_template::SEAL_KEY_NAMESPACE),
        &[
            "delete",
            "secret",
            crate::fiberd_template::SEAL_KEY_SECRET,
            "--ignore-not-found=true",
        ],
    )?;
    let mut namespaces = BTreeSet::from([SYSTEM.to_owned()]);
    namespaces.extend(delete_inventory(
        kube,
        read_core_inventory(kube, installation_id)?,
    )?);
    delete_namespaces(kube, namespaces)?;
    Ok(())
}

fn delete_namespaces(kube: &Kube<'_>, namespaces: BTreeSet<String>) -> Result<()> {
    let mut command = kube.command(None)?;
    command
        .args(["delete", "namespace"])
        .args(&namespaces)
        .args(["--ignore-not-found=true", "--wait=true", "--timeout=300s"]);
    let output = kube.host.output(&mut command)?;
    if output.exit == 0 {
        return Ok(());
    }
    for _ in 0..30 {
        let remaining = kube.capture(
            kube.command(None)?
                .args(["get", "namespace"])
                .args(&namespaces)
                .args(["--ignore-not-found=true", "--output=name"]),
        )?;
        if remaining.is_empty() {
            return Ok(());
        }
        ensure!(
            !kube.host.cancellation.is_cancelled(),
            "namespace deletion cancelled"
        );
        std::thread::sleep(Duration::from_secs(2));
    }
    kube.host.require_success(&command, &output)
}

pub(crate) fn remove_installation_identity(kube: &Kube<'_>) -> Result<()> {
    run(
        kube,
        Some("kube-system"),
        &[
            "delete",
            "configmap",
            "lampyr-install",
            "--ignore-not-found=true",
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crd_kinds_reads_one_group_and_kind_a_line() {
        assert!(crd_kinds("").is_empty());
        assert_eq!(
            crd_kinds("fiberd.io\tCapacityGrant\n"),
            BTreeSet::from([("fiberd.io".to_owned(), "CapacityGrant".to_owned())])
        );
        assert_eq!(
            crd_kinds("no tab here\ncert-manager.io\tCertificate"),
            BTreeSet::from([("cert-manager.io".to_owned(), "Certificate".to_owned())])
        );
    }

    #[test]
    fn a_reinstall_deletes_the_grant_issuer_it_recorded_and_nothing_reused() {
        // The grant issuer's manifest as main last installed it, recorded
        // as install records it.
        let issuer = r#"
apiVersion: v1
kind: ServiceAccount
metadata: {name: grant-issuer, namespace: lampyr-system}
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata: {name: lampyr-grant-issuer}
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRoleBinding
metadata: {name: lampyr-grant-issuer}
---
apiVersion: v1
kind: ServiceAccount
metadata: {name: fiberd-grant, namespace: default}
---
apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata: {name: lampyr-grant-home, namespace: default}
---
apiVersion: rbac.authorization.k8s.io/v1
kind: RoleBinding
metadata: {name: lampyr-grant-home, namespace: default}
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata: {name: lampyr-grant-namespace}
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRoleBinding
metadata: {name: lampyr-grant-namespace}
---
apiVersion: v1
kind: ServiceAccount
metadata: {name: fiber-router, namespace: lampyr-system}
---
apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata: {name: lampyr-router-grants, namespace: default}
---
apiVersion: rbac.authorization.k8s.io/v1
kind: RoleBinding
metadata: {name: lampyr-router-grants, namespace: default}
---
apiVersion: apps/v1
kind: Deployment
metadata: {name: grant-issuer, namespace: lampyr-system}
---
apiVersion: v1
kind: Service
metadata: {name: grant-issuer, namespace: lampyr-system}
---
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata: {name: lampyr-grant-homes, namespace: default}
"#;
        let mut inventory = Vec::new();
        for item in manifest_items(issuer).unwrap() {
            record_manifest(&item, &mut inventory).unwrap();
        }
        let cert_manager = json!({"apiVersion":"v1","kind":"Namespace",
            "metadata":{"name":"cert-manager"}});
        inventory.push(cert_manager.clone());
        let retired = take_retired(&mut inventory);
        assert_eq!(retired.len(), RETIRED.len());
        assert_eq!(
            retired[0],
            json!({"apiVersion":"apps/v1","kind":"Deployment",
                "metadata":{"name":"grant-issuer","namespace":"lampyr-system"}}),
            "the issuer stops first"
        );
        let kept: Vec<_> = inventory
            .iter()
            .map(|item| {
                format!(
                    "{}/{}",
                    item["kind"].as_str().unwrap(),
                    item["metadata"]["name"].as_str().unwrap()
                )
            })
            .collect();
        assert_eq!(
            kept,
            [
                "ServiceAccount/fiberd-grant",
                "ServiceAccount/fiber-router",
                "Role/lampyr-router-grants",
                "RoleBinding/lampyr-router-grants",
                "NetworkPolicy/lampyr-grant-homes",
                "Namespace/cert-manager",
            ]
        );
        assert!(take_retired(&mut inventory).is_empty());
        // No current manifest applies a retired object, which a reinstall
        // would otherwise delete and apply again.
        let k8s = Path::new(env!("CARGO_MANIFEST_DIR")).join("../k8s");
        for entry in std::fs::read_dir(k8s).unwrap() {
            let source = std::fs::read_to_string(entry.unwrap().path()).unwrap();
            let mut current = Vec::new();
            for item in manifest_items(&source).unwrap() {
                record_manifest(&item, &mut current).unwrap();
            }
            assert!(take_retired(&mut current).is_empty());
        }
    }

    #[test]
    fn the_csi_driver_runs_in_the_namespace_install_creates_for_it() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../k8s/cert-manager-csi-driver.yaml");
        let items = manifest_items(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert!(items.iter().all(|item| item["kind"] != "Namespace"));
        let namespaced: Vec<_> = items
            .iter()
            .filter_map(|item| item["metadata"]["namespace"].as_str())
            .collect();
        assert!(!namespaced.is_empty());
        assert!(
            namespaced
                .iter()
                .all(|namespace| *namespace == CSI_DRIVER_NAMESPACE),
            "{namespaced:?}"
        );
    }

    #[test]
    fn a_reinstall_deletes_only_the_retired_issuer_s_homes() {
        // Label sets as the issuer's homes and fiberd-k8s-controller's
        // grant Pods carry them, matched as the API server matches
        // RETIRED_HOME_SELECTOR.
        let matches = |labels: &[(&str, &str)]| {
            let labels: BTreeMap<_, _> = labels.iter().copied().collect();
            RETIRED_HOME_SELECTOR.split(',').all(|requirement| {
                if let Some(key) = requirement.strip_prefix('!') {
                    !labels.contains_key(key)
                } else if let Some((key, value)) = requirement.split_once("!=") {
                    labels.get(key) != Some(&value)
                } else if let Some((key, value)) = requirement.split_once('=') {
                    labels.get(key) == Some(&value)
                } else {
                    labels.contains_key(requirement)
                }
            })
        };
        let old_home = [
            ("app", "fiberd"),
            ("lampyr.dev/home", "true"),
            ("serving.knative.dev/revision", "demo-00001"),
            ("serving.knative.dev/service", "demo"),
            ("fiberd.io/grant", "demo-00001"),
        ];
        assert!(matches(&old_home));
        let mut grant_pod = old_home.to_vec();
        grant_pod.push(("app.kubernetes.io/managed-by", "fiberd-k8s-controller"));
        grant_pod.push(("fiberd.io/grant-uid", "0b5e6c1a"));
        assert!(!matches(&grant_pod));
        assert!(!matches(&grant_pod[..grant_pod.len() - 1]));
        // A home whose CapacityGrant the issuer never served is no Pod it
        // made.
        assert!(!matches(&old_home[..4]));
    }

    #[test]
    fn pinned_manifests_must_match_their_checksum() {
        let error = check_pinned_manifest(CERT_MANAGER, "kind: Namespace\n").unwrap_err();
        assert!(error.to_string().contains("not the pinned"), "{error}");
        check_pinned_manifest(&format!("{SERVING}/serving-crds.yaml"), "anything").unwrap();
        check_pinned_manifest("k8s/fiber-router.yaml", "anything").unwrap();
    }

    #[test]
    fn device_capacity_accepts_canonical_kubernetes_quantities() {
        for value in ["2000", "2k", "1Ki", "1e3"] {
            assert!(super::positive_quantity(value).unwrap(), "{value}");
        }
        assert!(!super::positive_quantity("0").unwrap());
        for value in ["", "invalid", "-1", "NaN"] {
            assert!(super::positive_quantity(value).is_err(), "{value}");
        }
    }

    #[test]
    fn core_inventory_records_only_stable_resource_identities() {
        let source = r#"
apiVersion: v1
kind: ConfigMap
metadata:
  name: settings
  namespace: default
data:
  mutable: value
---
apiVersion: v1
kind: Namespace
metadata:
  name: owned
"#;
        let mut inventory = Vec::new();
        for item in manifest_items(source).unwrap() {
            record_manifest(&item, &mut inventory).unwrap();
        }
        for item in manifest_items(source).unwrap() {
            record_manifest(&item, &mut inventory).unwrap();
        }
        assert_eq!(
            inventory,
            vec![
                json!({"apiVersion":"v1","kind":"ConfigMap",
                    "metadata":{"name":"settings","namespace":"default"}}),
                json!({"apiVersion":"v1","kind":"Namespace",
                    "metadata":{"name":"owned"}}),
            ]
        );
        let installation_id = "a".repeat(32);
        let encoded = encode_core_inventory(&installation_id, inventory.clone()).unwrap();
        assert_eq!(
            decode_core_inventory(&encoded, &installation_id).unwrap(),
            inventory
        );
        assert!(
            decode_core_inventory(&encoded, &"b".repeat(32))
                .unwrap_err()
                .to_string()
                .contains("another installation")
        );
    }

    #[test]
    fn admission_policies_are_deleted_before_the_objects_they_guard() {
        let item = |kind: &str| json!({ "kind": kind, "metadata": { "name": "default" } });
        let remaining = vec![
            (item("Deployment"), false),
            (item("GrantPolicy"), true),
            (item("ValidatingAdmissionPolicyBinding"), false),
            (item("ValidatingAdmissionPolicy"), false),
        ];
        let kinds = deletion_passes(&remaining).map(|pass| {
            pass.iter()
                .map(|item| item["kind"].as_str().unwrap())
                .collect::<Vec<_>>()
        });
        assert_eq!(
            kinds,
            [
                vec![
                    "ValidatingAdmissionPolicyBinding",
                    "ValidatingAdmissionPolicy"
                ],
                vec!["GrantPolicy"],
                vec!["Deployment"],
            ]
        );
    }

    #[test]
    fn grant_policies_list_the_repository_of_a_pinned_grant_image() {
        let digest = format!("sha256:{}", "a".repeat(64));
        assert_eq!(
            image_repository(
                "grantImage",
                &format!("localhost:5001/fiberd:pr101@{digest}")
            )
            .unwrap(),
            "localhost:5001/fiberd"
        );
        assert_eq!(
            image_repository("grantImage", &format!("ghcr.io/azure/fiberd@{digest}")).unwrap(),
            "ghcr.io/azure/fiberd"
        );
        for image in [
            "ghcr.io/team/helper:v1",
            "ghcr.io/team/helper@sha256:00",
            "ghcr.io/team/helper@sha512:00",
        ] {
            let error = image_repository("helperImage", image).unwrap_err();
            assert!(
                error.to_string().contains("helperImage"),
                "{image}: {error}"
            );
        }
        let upper = format!("ghcr.io/team/helper@sha256:{}", "A".repeat(64));
        assert!(image_repository("helperImage", &upper).is_err());
        // The GrantPolicy's ImageRepositoryPattern refuses these: a
        // registry without a dot or port, which it takes for a Docker Hub
        // path, an uppercase path, a scheme, a bare registry, an overlong
        // repository.
        for name in [
            "registry/fiberd".to_owned(),
            "fiberd".to_owned(),
            "ghcr.io/Team/fiberd".to_owned(),
            "http://registry.local:5000/fiberd".to_owned(),
            "https://ghcr.io/team/fiberd".to_owned(),
            "ghcr.io".to_owned(),
            format!("ghcr.io/{}", "a".repeat(250)),
        ] {
            let error = image_repository("grantImage", &format!("{name}@{digest}")).unwrap_err();
            assert!(
                error.to_string().contains("fully qualified repository"),
                "{name}: {error}"
            );
        }
        assert_eq!(
            image_repository("grantImage", &format!("registry:5000/fiberd:v1@{digest}")).unwrap(),
            "registry:5000/fiberd"
        );
    }

    #[test]
    fn serving_core_omits_stock_execution_resources() {
        let source = r#"
apiVersion: apps/v1
kind: Deployment
metadata: {name: controller, namespace: knative-serving}
spec: {replicas: 1}
---
apiVersion: apps/v1
kind: Deployment
metadata: {name: activator, namespace: knative-serving}
spec: {replicas: 1}
---
apiVersion: apps/v1
kind: Deployment
metadata: {name: autoscaler, namespace: knative-serving}
spec: {replicas: 1}
---
apiVersion: apps/v1
kind: Deployment
metadata: {name: webhook, namespace: knative-serving}
spec: {replicas: 1}
---
apiVersion: autoscaling/v2
kind: HorizontalPodAutoscaler
metadata: {name: activator, namespace: knative-serving}
"#;
        let core = minimal_serving_core(source).unwrap();
        let items = core["items"].as_array().unwrap();
        for name in ["controller", "activator", "autoscaler"] {
            assert!(
                !items
                    .iter()
                    .any(|item| item["kind"] == "Deployment" && item["metadata"]["name"] == name)
            );
        }

        assert!(items.iter().all(|item| {
            item["kind"] != "HorizontalPodAutoscaler" || item["metadata"]["name"] != "activator"
        }));
        assert_eq!(
            items
                .iter()
                .find(|item| {
                    item["kind"] == "Deployment" && item["metadata"]["name"] == "webhook"
                })
                .unwrap()["spec"]["replicas"],
            1
        );
    }

    #[test]
    fn fiber_defaults_disable_unenforced_timeout_variants() {
        let defaults = fiber_serving_defaults();
        assert_eq!(
            defaults["data"]["revision-response-start-timeout-seconds"],
            "0"
        );
        assert_eq!(defaults["data"]["revision-idle-timeout-seconds"], "0");
    }

    #[test]
    fn webhook_endpoint_requires_a_ready_address() {
        assert!(!has_ready_endpoint(&json!({"subsets":[{"addresses":[]}]})));
        assert!(has_ready_endpoint(&json!({
            "subsets":[{"addresses":[{"ip":"10.0.0.1"}]}]
        })));
    }

    #[test]
    fn only_webhook_startup_failures_are_retryable() {
        let starting = anyhow::anyhow!(
            "failed calling webhook config.webhook.serving.knative.dev: connection refused"
        );
        assert!(webhook_is_starting(&starting));
        let rejected = anyhow::anyhow!(
            "failed calling webhook config.webhook.serving.knative.dev: denied the request"
        );
        assert!(!webhook_is_starting(&rejected));
        // Only cert-manager's webhook, whose CA bundle cainjector injects
        // after it starts, retries an unknown authority.
        let uninjected = anyhow::anyhow!(
            "Internal error occurred: failed calling webhook \"webhook.cert-manager.io\": \
             failed to call webhook: Post \"https://cert-manager-webhook.cert-manager.svc:443/\
             validate?timeout=30s\": tls: failed to verify certificate: x509: certificate \
             signed by unknown authority"
        );
        assert!(cert_manager_webhook_is_starting(&uninjected));
        assert!(!webhook_is_starting(&uninjected));
        assert!(cert_manager_webhook_is_starting(&starting));
        let other = anyhow::anyhow!(
            "failed calling webhook \"config.webhook.serving.knative.dev\": x509: certificate \
             signed by unknown authority"
        );
        assert!(!cert_manager_webhook_is_starting(&other));
        assert!(!cert_manager_webhook_is_starting(&rejected));
    }

    #[test]
    fn uninstall_deletes_domain_mappings_before_serving_inventory() {
        assert_eq!(
            SERVING_RESOURCES_FOR_CLEANUP.first().copied(),
            Some("domainmappings.serving.knative.dev")
        );
    }
}
