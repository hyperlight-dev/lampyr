// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::path::Path;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::{Datelike, Utc};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, pem::PemObject};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::kube::{Kube, SYSTEM};

const BUNDLED_SOURCES: &str = include_str!("../../k8s/upstream/sources.json");
const BUNDLED_MANIFESTS: &[&str] = &[
    "serving-crds.yaml",
    "serving-core.yaml",
    "kourier.yaml",
    "cert-manager.yaml",
];
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
    pub(crate) ingress: &'a crate::platform_host::IngressSettings,
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

fn bundled_manifest(root: &Path, name: &str) -> Result<String> {
    use sha2::Digest as _;
    ensure!(
        BUNDLED_MANIFESTS.contains(&name),
        "unknown bundled manifest {name}"
    );
    let relative = format!("k8s/upstream/{name}");
    let sources: Value = serde_json::from_str(BUNDLED_SOURCES)?;
    let expected = sources["manifests"]
        .as_array()
        .context("bundled manifest source inventory is invalid")?
        .iter()
        .find(|manifest| manifest["path"] == relative)
        .and_then(|manifest| manifest["sha256"].as_str())
        .context("bundled manifest checksum is missing")?;
    let path = root.join(&relative);
    let source = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "read bundled platform manifest {}; restore the platform bundle",
            path.display()
        )
    })?;
    let digest: String = sha2::Sha256::digest(source.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    ensure!(
        digest == expected,
        "bundled platform manifest {} has SHA-256 {digest}, not the pinned {expected}; restore the platform bundle",
        path.display()
    );
    Ok(source)
}

fn apply_bundled_manifest(
    kube: &Kube<'_>,
    options: &Options<'_>,
    name: &str,
    inventory: &mut Vec<Value>,
) -> Result<()> {
    let source = bundled_manifest(options.root, name)?;
    apply_recorded_text_with(kube, options.installation_id, &source, inventory, &[])
}

fn manifest_source(kube: &Kube<'_>, source: &str) -> Result<String> {
    if source.starts_with("https://") {
        crate::platform::download_text(source, 8 * 1024 * 1024, kube.host)
            .context("download platform manifest")
    } else {
        std::fs::read_to_string(source).with_context(|| format!("read {source}"))
    }
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
        if item["metadata"]["annotations"]["lampyr.dev/credential-boundary"] == "true" {
            metadata["annotations"] = json!({"lampyr.dev/credential-boundary":"true"});
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
    apply_recorded_text_with(kube, installation_id, &source, inventory, flags)
}

fn apply_recorded_text_with(
    kube: &Kube<'_>,
    installation_id: &str,
    source: &str,
    inventory: &mut Vec<Value>,
    flags: &[&str],
) -> Result<()> {
    let items = manifest_items(source)?;
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
    for &name in BUNDLED_MANIFESTS {
        bundled_manifest(options.root, name)?;
    }
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
            apply_bundled_manifest(kube, options, "serving-crds.yaml", inventory)?;
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
            let source = bundled_manifest(options.root, "serving-core.yaml")?;
            let core = minimal_serving_core(&source)?;
            apply_recorded_manifest(kube, options.installation_id, &core, inventory)?;
            rollout(kube, "knative-serving", "deployment", "webhook", "180s")?;
            wait_for_webhook_endpoint(kube)?;
            patch_fiber_serving_defaults(kube)
        })?;
    kube.host.step("Installing certificate management", || {
        install_cert_manager(kube, options, inventory)
    })?;
    kube.host
        .step("Wiring your network and HTTPS ingress", || {
            apply_bundled_manifest(kube, options, "kourier.yaml", inventory)?;
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
            configure_ingress_service(kube, options)?;
            if options.ingress_ports.is_none() {
                let expected = options
                    .ingress
                    .service
                    .as_ref()
                    .map(|service| service.public_ip_address.as_str());
                wait_ingress_address(kube, expected, Duration::from_secs(600))?;
            }
            if options.ingress.service.is_some() {
                wait_dns(
                    kube,
                    options.domain,
                    options
                        .ingress
                        .service
                        .as_ref()
                        .unwrap()
                        .public_ip_address
                        .parse()?,
                    options.https_port,
                    Duration::from_secs(600),
                )?;
            }
            let (secret_namespace, secret_name) = configure_ingress_tls(kube, options, inventory)?;
            kube.patch(
                "knative-serving",
                "configmap",
                "config-kourier",
                &json!({ "data": {
                    "certs-secret-name": secret_name, "certs-secret-namespace": secret_namespace
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
            )?;
            if !matches!(
                &options.ingress.tls,
                crate::platform_host::IngressTls::SelfSigned
            ) {
                let tls = wait_tls_secret(
                    kube,
                    &secret_namespace,
                    &secret_name,
                    options.domain,
                    Duration::from_secs(30),
                )?;
                wait_tls_endpoint(
                    kube,
                    options.domain,
                    options.https_port,
                    tls,
                    Duration::from_secs(120),
                )?;
            }
            Ok(())
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
    let (tls_mode, secret_namespace, secret_name, issuer, credential) =
        ingress_metadata(options.ingress)?;
    let dns_address = options
        .ingress
        .service
        .as_ref()
        .map(|service| Value::String(service.public_ip_address.clone()))
        .unwrap_or(Value::Null);
    kube.patch("kube-system", "configmap", "lampyr-install", &json!({ "data": {
        "version": env!("CARGO_PKG_VERSION"), "domain": options.domain, "httpsPort": options.https_port.to_string(),
        "fiberdImage": options.fiberd_image, "routerImage": options.router_image,
        "grantImage": options.grant_image, "helperImage": options.helper_image,
        "templateRepository": options.template_repository,
        "ingressTlsMode": tls_mode, "ingressTlsSecretNamespace": secret_namespace,
        "ingressTlsSecretName": secret_name, "ingressTlsIssuer": issuer,
        "ingressDnsAddress": dns_address, "ingressCredential": credential,
        "ingressRetiringCredential": Value::Null,
        "ingressServiceAnnotations": serde_json::to_string(
            &options.ingress.service.as_ref()
                .map(|service| service.annotations.clone())
                .unwrap_or_default()
        )?
    } }))?;
    Ok(())
}

fn configure_ingress_service(kube: &Kube<'_>, options: &Options<'_>) -> Result<()> {
    if let Some(ports) = &options.ingress_ports {
        ensure!(ports.is_array(), "ingress ports must be an array");
        kube.patch(
            "kourier-system",
            "service",
            "kourier",
            &json!({ "spec": { "ports": ports } }),
        )?;
    }
    let service = options.ingress.service.as_ref();
    let previous_annotations: BTreeMap<String, String> = kube
        .optional("kube-system", "configmap", "lampyr-install")?
        .and_then(|install| {
            install["data"]["ingressServiceAnnotations"]
                .as_str()
                .map(str::to_owned)
        })
        .map(|value| serde_json::from_str(&value))
        .transpose()
        .context("decode installed ingress Service annotations")?
        .unwrap_or_default();
    let mut annotations = serde_json::Map::new();
    for key in previous_annotations.keys() {
        annotations.insert(key.clone(), Value::Null);
    }
    if let Some(service) = service {
        for (key, value) in &service.annotations {
            annotations.insert(key.clone(), value.clone().into());
        }
    }
    kube.patch(
        "kourier-system",
        "service",
        "kourier",
        &json!({
            "metadata": { "annotations": annotations },
            "spec": { "loadBalancerIP":
                service.map(|value| Value::String(value.public_ip_address.clone())).unwrap_or(Value::Null)
            }
        }),
    )?;
    Ok(())
}

fn install_cert_manager(
    kube: &Kube<'_>,
    options: &Options<'_>,
    inventory: &mut Vec<Value>,
) -> Result<()> {
    if kube
        .optional("default", "crd", "certificates.cert-manager.io")?
        .is_none()
    {
        apply_bundled_manifest(kube, options, "cert-manager.yaml", inventory)?;
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
    Ok(())
}

/// Installs fiberd-k8s-controller and makes namespace default its tenant:
/// the controller, the GrantPolicy that admits Lampyr's grant Pods, and the
/// tenant objects the controller's config/tenant would add but its author
/// RoleBinding: Lampyr's serving controller writes every CapacityGrant in
/// default under its own ClusterRole, so no group of people needs to.
fn install_controller(
    kube: &Kube<'_>,
    options: &Options<'_>,
    inventory: &mut Vec<Value>,
) -> Result<()> {
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
    // The runtime ceiling admits only Hyperlight. No runtime is allowed by
    // default; only the tenant namespace gets the Hyperlight exception.
    // The pinned controller does not allow privileged grant Pods.
    let policy = json!({
        "apiVersion": "fiberd.io/v1alpha1", "kind": "GrantPolicy",
        "metadata": { "name": "default" },
        "spec": {
            "images": [image_repository("grantImage", options.grant_image)?],
            "helperImages": [image_repository("helperImage", options.helper_image)?],
            "registries": options.zygote_registries,
            "runtimes": ["hyperlight"],
            "defaultRuntimes": [],
        }
    });
    apply_recorded_manifest(kube, options.installation_id, &policy, inventory)?;
    kube.patch(
        "default",
        "namespace",
        "default",
        &json!({ "metadata": { "labels": { "runtimes.fiberd.io/hyperlight": "true" } } }),
    )?;
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

fn wait_ingress_address(kube: &Kube<'_>, expected: Option<&str>, timeout: Duration) -> Result<()> {
    kube.wait("public ingress address", timeout, || {
        let service: Value = kube.get("kourier-system", "service", "kourier")?;
        let addresses = service
            .pointer("/status/loadBalancer/ingress")
            .and_then(Value::as_array);
        Ok(addresses.and_then(|addresses| {
            addresses
                .iter()
                .any(|address| {
                    expected.map_or_else(
                        || {
                            address["ip"].as_str().is_some()
                                || address["hostname"].as_str().is_some()
                        },
                        |expected| address["ip"] == expected,
                    )
                })
                .then_some(())
        }))
    })
}

struct AddressResolver {
    request: mpsc::SyncSender<u64>,
    result: mpsc::Receiver<(u64, std::io::Result<Vec<SocketAddr>>)>,
    next: u64,
    pending: Option<u64>,
}

impl AddressResolver {
    fn new(hostname: &str, port: u16) -> Self {
        let hostname = hostname.to_owned();
        let (request, requests) = mpsc::sync_channel(1);
        let (results, result) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            while let Ok(sequence) = requests.recv() {
                let addresses = (hostname.as_str(), port)
                    .to_socket_addrs()
                    .map(|addresses| addresses.collect());
                if results.send((sequence, addresses)).is_err() {
                    break;
                }
            }
        });
        Self {
            request,
            result,
            next: 0,
            pending: None,
        }
    }

    fn resolve(&mut self, timeout: Duration) -> Result<Option<Vec<SocketAddr>>> {
        let sequence = if let Some(sequence) = self.pending {
            sequence
        } else {
            self.next = self.next.wrapping_add(1);
            let sequence = self.next;
            match self.request.try_send(sequence) {
                Ok(()) => self.pending = Some(sequence),
                Err(mpsc::TrySendError::Disconnected(_)) => {
                    anyhow::bail!("DNS resolver stopped")
                }
                Err(mpsc::TrySendError::Full(_)) => {
                    anyhow::bail!("DNS resolver request state is inconsistent")
                }
            }
            sequence
        };
        let deadline = Instant::now() + timeout;
        loop {
            match self
                .result
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok((received, result)) if received == sequence => {
                    self.pending = None;
                    return Ok(result.ok());
                }
                Ok(_) => {}
                Err(mpsc::RecvTimeoutError::Timeout) => return Ok(None),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    anyhow::bail!("DNS resolver stopped")
                }
            }
        }
    }
}

fn wait_dns(
    kube: &Kube<'_>,
    domain: &str,
    expected: IpAddr,
    port: u16,
    timeout: Duration,
) -> Result<()> {
    let hostname = format!("lampyr-readiness.default.{domain}");
    let mut resolver = AddressResolver::new(&hostname, port);
    kube.wait(
        &format!("DNS {hostname}:{port} resolving to {expected}"),
        timeout,
        || {
            let addresses = match resolver.resolve(kube.timeout(Duration::from_secs(2))?)? {
                Some(addresses) => addresses,
                None => return Ok(None),
            };
            Ok(addresses
                .iter()
                .any(|address| address.ip() == expected)
                .then_some(()))
        },
    )
}

fn ingress_metadata(
    ingress: &crate::platform_host::IngressSettings,
) -> Result<(String, String, String, Value, Value)> {
    use crate::platform_host::IngressTls;
    Ok(match &ingress.tls {
        IngressTls::SelfSigned => (
            "selfSigned".into(),
            "kourier-system".into(),
            "lampyr-kourier-tls".into(),
            Value::Null,
            Value::Null,
        ),
        IngressTls::ExistingSecret {
            secret_name,
            secret_namespace,
        } => (
            "existingSecret".into(),
            secret_namespace.clone(),
            secret_name.clone(),
            Value::Null,
            Value::Null,
        ),
        IngressTls::Acme {
            issuer, credential, ..
        } => (
            "acme".into(),
            "kourier-system".into(),
            "lampyr-kourier-tls".into(),
            Value::String(issuer.clone()),
            Value::String(serde_json::to_string(credential)?),
        ),
    })
}

fn configure_ingress_tls(
    kube: &Kube<'_>,
    options: &Options<'_>,
    inventory: &mut Vec<Value>,
) -> Result<(String, String)> {
    use crate::platform_host::IngressTls;
    let installed_credentials = installed_cert_manager_credentials(kube)?;
    match &options.ingress.tls {
        IngressTls::SelfSigned => {
            let previous_credentials = installed_credentials.previous_for(None);
            persist_cert_manager_credentials(kube, None, &previous_credentials)?;
            remove_acme_resources(kube, options.installation_id, inventory)?;
            configure_cert_manager_credential(kube, inventory, None, &previous_credentials)?;
            remove_retired_credential_boundaries(
                kube,
                options.installation_id,
                &previous_credentials,
                None,
                inventory,
            )?;
            persist_cert_manager_credentials(kube, None, &[])?;
            self_signed_tls(kube, options.domain, options.installation_id)?;
            Ok(("kourier-system".into(), "lampyr-kourier-tls".into()))
        }
        IngressTls::ExistingSecret {
            secret_name,
            secret_namespace,
        } => {
            wait_tls_secret(
                kube,
                secret_namespace,
                secret_name,
                options.domain,
                Duration::from_secs(120),
            )?;
            let previous_credentials = installed_credentials.previous_for(None);
            persist_cert_manager_credentials(kube, None, &previous_credentials)?;
            remove_acme_resources(kube, options.installation_id, inventory)?;
            configure_cert_manager_credential(kube, inventory, None, &previous_credentials)?;
            remove_retired_credential_boundaries(
                kube,
                options.installation_id,
                &previous_credentials,
                None,
                inventory,
            )?;
            persist_cert_manager_credentials(kube, None, &[])?;
            Ok((secret_namespace.clone(), secret_name.clone()))
        }
        IngressTls::Acme {
            email,
            issuer,
            solver,
            credential,
        } => {
            ensure!(
                cert_manager_owned(inventory),
                "ACME ingress requires the cert-manager installation managed by Lampyr"
            );
            let previous_credentials =
                installed_credentials.previous_for(Some(credential.as_ref()));
            persist_cert_manager_credentials(
                kube,
                Some(credential.as_ref()),
                &previous_credentials,
            )?;
            configure_credential_boundary(kube, options.installation_id, credential, inventory)?;
            wait_credential_boundary(kube, credential)?;
            if let Err(error) = ensure_no_external_credential_resources(kube, credential) {
                let active_credentials = installed_credentials.previous_for(None);
                configure_cert_manager_credential(kube, inventory, None, &active_credentials)
                    .context("disable cert-manager identity after unsafe Issuer discovery")?;
                return Err(error);
            }
            configure_cert_manager_credential(
                kube,
                inventory,
                Some(credential),
                &previous_credentials,
            )?;
            remove_retired_credential_boundaries(
                kube,
                options.installation_id,
                &previous_credentials,
                Some(credential),
                inventory,
            )?;
            persist_cert_manager_credentials(kube, Some(credential.as_ref()), &[])?;
            let issuer_manifest = json!({
                "apiVersion":"cert-manager.io/v1", "kind":"Issuer",
                "metadata":{"name":issuer,"namespace":"kourier-system","annotations":{
                    "lampyr.dev/installation-id":options.installation_id
                }},
                "spec":{"acme":{
                    "email":email,
                    "server":"https://acme-v02.api.letsencrypt.org/directory",
                    "privateKeySecretRef":{"name":"lampyr-acme-account"},
                    "solvers":[solver]
                }}
            });
            ensure_owned_tls_resource(
                kube,
                Some("kourier-system"),
                "issuer",
                issuer,
                options.installation_id,
            )?;
            apply_cert_manager_manifest(
                kube,
                options.installation_id,
                "ACME Issuer admission",
                &issuer_manifest,
                inventory,
            )?;
            wait_ready_resource(
                kube,
                "kourier-system",
                "issuer",
                issuer,
                "ACME issuer",
                Duration::from_secs(300),
            )?;
            ensure_owned_tls_secret(kube, options.installation_id)?;
            let certificate = json!({
                "apiVersion":"cert-manager.io/v1", "kind":"Certificate",
                "metadata":{"name":"lampyr-kourier-tls","namespace":"kourier-system",
                    "annotations":{"lampyr.dev/installation-id":options.installation_id}},
                "spec":{
                    "secretName":"lampyr-kourier-tls",
                    "secretTemplate":{"annotations":{
                        "lampyr.dev/installation-id":options.installation_id,
                        "lampyr.dev/domain":options.domain,
                        "lampyr.dev/tls-mode":"acme"
                    }},
                    "dnsNames":[format!("*.{}",options.domain),format!("*.default.{}",options.domain)],
                    "issuerRef":{"group":"cert-manager.io","kind":"Issuer","name":issuer}
                }
            });
            ensure_owned_tls_resource(
                kube,
                Some("kourier-system"),
                "certificate",
                "lampyr-kourier-tls",
                options.installation_id,
            )?;
            apply_cert_manager_manifest(
                kube,
                options.installation_id,
                "ingress Certificate admission",
                &certificate,
                inventory,
            )?;
            wait_ready_resource(
                kube,
                "kourier-system",
                "certificate",
                "lampyr-kourier-tls",
                "ingress Certificate",
                Duration::from_secs(900),
            )?;
            wait_tls_secret(
                kube,
                "kourier-system",
                "lampyr-kourier-tls",
                options.domain,
                Duration::from_secs(30),
            )?;
            remove_retired_issuers(kube, options.installation_id, issuer, inventory)?;
            Ok(("kourier-system".into(), "lampyr-kourier-tls".into()))
        }
    }
}

fn cert_manager_owned(inventory: &[Value]) -> bool {
    inventory.iter().any(|item| {
        item["apiVersion"] == "apps/v1"
            && item["kind"] == "Deployment"
            && item["metadata"]["namespace"] == "cert-manager"
            && item["metadata"]["name"] == "cert-manager"
    })
}

#[derive(Default)]
struct InstalledCertManagerCredentials {
    current: Option<crate::platform_host::CertManagerCredential>,
    retiring: Vec<crate::platform_host::CertManagerCredential>,
}

impl InstalledCertManagerCredentials {
    fn previous_for(
        &self,
        current: Option<&crate::platform_host::CertManagerCredential>,
    ) -> Vec<crate::platform_host::CertManagerCredential> {
        let mut previous = Vec::new();
        for credential in self.current.iter().chain(&self.retiring) {
            if current.is_some_and(|current| current == credential)
                || previous.iter().any(|existing| existing == credential)
            {
                continue;
            }
            previous.push(credential.clone());
        }
        previous
    }
}

fn persist_cert_manager_credentials(
    kube: &Kube<'_>,
    current: Option<&crate::platform_host::CertManagerCredential>,
    retiring: &[crate::platform_host::CertManagerCredential],
) -> Result<()> {
    kube.patch(
        "kube-system",
        "configmap",
        "lampyr-install",
        &cert_manager_credential_state_patch(current, retiring)?,
    )
}

fn cert_manager_credential_state_patch(
    current: Option<&crate::platform_host::CertManagerCredential>,
    retiring: &[crate::platform_host::CertManagerCredential],
) -> Result<Value> {
    let current = current
        .map(serde_json::to_string)
        .transpose()?
        .map(Value::String)
        .unwrap_or(Value::Null);
    let retiring = if retiring.is_empty() {
        Value::Null
    } else {
        Value::String(serde_json::to_string(retiring)?)
    };
    Ok(json!({"data":{
        "ingressCredential":current,
        "ingressRetiringCredential":retiring
    }}))
}

fn apply_cert_manager_manifest(
    kube: &Kube<'_>,
    installation_id: &str,
    label: &str,
    manifest: &Value,
    inventory: &mut Vec<Value>,
) -> Result<()> {
    let mut last_retryable = None;
    let result = kube.wait(
        label,
        Duration::from_secs(120),
        || match apply_recorded_manifest(kube, installation_id, manifest, inventory) {
            Ok(()) => Ok(Some(())),
            Err(error) if cert_manager_webhook_is_starting(&error) => {
                last_retryable = Some(format!("{error:#}"));
                Ok(None)
            }
            Err(error) => Err(error),
        },
    );
    match (result, last_retryable) {
        (Err(error), Some(last)) => {
            Err(error).context(format!("last retryable admission error: {last}"))
        }
        (result, _) => result,
    }
}

fn installed_cert_manager_credentials(kube: &Kube<'_>) -> Result<InstalledCertManagerCredentials> {
    let Some(install) = kube.optional("kube-system", "configmap", "lampyr-install")? else {
        return Ok(InstalledCertManagerCredentials::default());
    };
    decode_installed_cert_manager_credentials(&install)
}

fn decode_installed_cert_manager_credentials(
    install: &Value,
) -> Result<InstalledCertManagerCredentials> {
    let current = install["data"]["ingressCredential"]
        .as_str()
        .map(serde_json::from_str)
        .transpose()
        .context("decode installed cert-manager credential configuration")?;
    let retiring = install["data"]["ingressRetiringCredential"]
        .as_str()
        .map(serde_json::from_str)
        .transpose()
        .context("decode retiring cert-manager credential configurations")?
        .unwrap_or_default();
    Ok(InstalledCertManagerCredentials { current, retiring })
}

fn cert_manager_controller_args(
    deployment: &Value,
    current: Option<&crate::platform_host::CertManagerCredential>,
    previous: &[crate::platform_host::CertManagerCredential],
) -> Result<Vec<Value>> {
    let container = deployment
        .pointer("/spec/template/spec/containers")
        .and_then(Value::as_array)
        .and_then(|containers| {
            containers
                .iter()
                .find(|container| container["name"] == "cert-manager-controller")
        })
        .context("cert-manager Deployment has no cert-manager-controller container")?;
    let mut args = container["args"]
        .as_array()
        .context("cert-manager controller args must be an array")?
        .clone();
    let prefixes: BTreeSet<_> = current
        .into_iter()
        .chain(previous.iter())
        .flat_map(|credential| credential.controller_arg_prefixes.iter())
        .collect();
    args.retain(|arg| {
        arg.as_str()
            .is_none_or(|arg| !prefixes.iter().any(|prefix| arg.starts_with(*prefix)))
    });
    if let Some(current) = current {
        for argument in &current.controller_args {
            if !args.iter().any(|existing| existing == argument) {
                args.push(argument.clone().into());
            }
        }
    }
    Ok(args)
}

fn merge_json(target: &mut Value, patch: &Value) {
    if let (Some(target), Some(patch)) = (target.as_object_mut(), patch.as_object()) {
        for (key, value) in patch {
            merge_json(target.entry(key.clone()).or_insert(Value::Null), value);
        }
    } else {
        *target = patch.clone();
    }
}

fn cert_manager_credential_patch(
    deployment: &Value,
    current: Option<&crate::platform_host::CertManagerCredential>,
    previous: &[crate::platform_host::CertManagerCredential],
) -> Result<Value> {
    let mut patch = json!({});
    for credential in previous {
        merge_json(&mut patch, &credential.deployment_cleanup_patch);
    }
    if let Some(current) = current {
        merge_json(&mut patch, &current.deployment_patch);
    }
    merge_json(
        &mut patch,
        &json!({"spec":{"template":{"spec":{"containers":[{
            "name":"cert-manager-controller",
            "args":cert_manager_controller_args(deployment, current, previous)?
        }]}}}}),
    );
    Ok(patch)
}

fn field_path_exists(value: &Value, path: &[String]) -> bool {
    let Some((head, tail)) = path.split_first() else {
        return !value.is_null();
    };
    if head == "*" {
        value
            .as_array()
            .is_some_and(|items| items.iter().any(|item| field_path_exists(item, tail)))
    } else {
        value
            .get(head)
            .is_some_and(|value| field_path_exists(value, tail))
    }
}

fn ensure_no_external_credential_resources(
    kube: &Kube<'_>,
    credential: &crate::platform_host::CertManagerCredential,
) -> Result<()> {
    for audit in &credential.audits {
        let mut command = kube.command(None)?;
        command.args([
            "get",
            &audit.resource,
            "--all-namespaces",
            "--output",
            "json",
        ]);
        let resources: Value = serde_json::from_str(&kube.capture(&mut command)?)
            .with_context(|| format!("decode cert-manager {}s", audit.kind))?;
        for item in resources["items"]
            .as_array()
            .with_context(|| format!("cert-manager {} list has no items", audit.kind))?
        {
            let namespace = item["metadata"]["namespace"].as_str().unwrap_or_default();
            let issuer_kind_matches = audit.issuer_kind.as_ref().is_none_or(|expected| {
                item.pointer("/spec/issuerRef/kind")
                    .and_then(Value::as_str)
                    .unwrap_or("Issuer")
                    == expected
            });
            if namespace != audit.allowed_namespace
                && issuer_kind_matches
                && field_path_exists(item, &audit.field_path)
            {
                let name = item["metadata"]["name"].as_str().unwrap_or("<unknown>");
                anyhow::bail!(
                    "{} {namespace}/{name} conflicts with the provider credential boundary",
                    audit.kind
                );
            }
        }
    }
    Ok(())
}

fn configure_credential_boundary(
    kube: &Kube<'_>,
    installation_id: &str,
    credential: &crate::platform_host::CertManagerCredential,
    inventory: &mut Vec<Value>,
) -> Result<()> {
    for item in credential.boundary["items"].as_array().unwrap() {
        ensure!(
            item.pointer("/metadata/annotations/lampyr.dev~1installation-id")
                .and_then(Value::as_str)
                == Some(installation_id)
                && item
                    .pointer("/metadata/annotations/lampyr.dev~1credential-boundary")
                    .and_then(Value::as_str)
                    == Some("true"),
            "credential boundary resources must carry Lampyr ownership annotations"
        );
        let kind = item["kind"]
            .as_str()
            .context("credential boundary kind is missing")?
            .to_ascii_lowercase();
        let name = item["metadata"]["name"]
            .as_str()
            .context("credential boundary name is missing")?;
        let namespace = item["metadata"]["namespace"].as_str();
        ensure_owned_tls_resource(kube, namespace, &kind, name, installation_id)?;
    }
    apply_recorded_manifest(kube, installation_id, &credential.boundary, inventory)
}

fn wait_credential_boundary(
    kube: &Kube<'_>,
    credential: &crate::platform_host::CertManagerCredential,
) -> Result<()> {
    for resource in &credential.probes {
        let kind = resource["kind"].as_str().unwrap_or("resource");
        let mut probe = tempfile::NamedTempFile::new()?;
        serde_json::to_writer(&mut probe, resource)?;
        let path = probe
            .path()
            .to_str()
            .context("credential boundary probe path must be UTF-8")?;
        kube.wait(
            &format!("{kind} credential admission boundary"),
            Duration::from_secs(30),
            || {
                let mut command = kube.command(None)?;
                command.args(["create", "--dry-run=server", "--filename", path]);
                match kube.capture(&mut command) {
                    Ok(_) => Ok(None),
                    Err(error) if format!("{error:#}").contains(&credential.boundary_message) => {
                        Ok(Some(()))
                    }
                    Err(error) => Err(error),
                }
            },
        )?;
    }
    Ok(())
}

fn configure_cert_manager_credential(
    kube: &Kube<'_>,
    inventory: &[Value],
    current: Option<&crate::platform_host::CertManagerCredential>,
    previous: &[crate::platform_host::CertManagerCredential],
) -> Result<()> {
    if current.is_some() {
        ensure!(
            cert_manager_owned(inventory),
            "managed ACME ingress requires the cert-manager installation managed by Lampyr"
        );
    }
    if current.is_some() || !previous.is_empty() {
        let mut service_account_patch = json!({});
        for credential in previous {
            merge_json(
                &mut service_account_patch,
                &credential.service_account_cleanup_patch,
            );
        }
        if let Some(current) = current {
            merge_json(&mut service_account_patch, &current.service_account_patch);
        }
        kube.patch(
            "cert-manager",
            "serviceaccount",
            "cert-manager",
            &service_account_patch,
        )?;
        let deployment: Value = kube.get("cert-manager", "deployment", "cert-manager")?;
        run(
            kube,
            Some("cert-manager"),
            &[
                "patch",
                "deployment",
                "cert-manager",
                "--type",
                "strategic",
                "--patch",
                &serde_json::to_string(&cert_manager_credential_patch(
                    &deployment,
                    current,
                    previous,
                )?)?,
            ],
        )?;
        rollout(kube, "cert-manager", "deployment", "cert-manager", "300s")?;
    }
    Ok(())
}

fn ensure_owned_tls_resource(
    kube: &Kube<'_>,
    namespace: Option<&str>,
    kind: &str,
    name: &str,
    installation_id: &str,
) -> Result<()> {
    if let Some(resource) = kube.optional(namespace.unwrap_or("default"), kind, name)? {
        ensure!(
            resource
                .pointer("/metadata/annotations/lampyr.dev~1installation-id")
                .and_then(Value::as_str)
                == Some(installation_id),
            "{kind}/{name} is not owned by this Lampyr installation"
        );
    }
    Ok(())
}

fn ensure_owned_tls_secret(kube: &Kube<'_>, installation_id: &str) -> Result<()> {
    if let Some(secret) = kube.optional("kourier-system", "secret", "lampyr-kourier-tls")? {
        let owner = secret
            .pointer("/metadata/annotations/lampyr.dev~1installation-id")
            .and_then(Value::as_str);
        let legacy = secret
            .pointer("/metadata/annotations/lampyr.dev~1domain")
            .and_then(Value::as_str)
            .is_some();
        ensure!(
            owner == Some(installation_id) || (owner.is_none() && legacy),
            "secret/lampyr-kourier-tls is not owned by this Lampyr installation"
        );
    }
    Ok(())
}

fn wait_ready_resource(
    kube: &Kube<'_>,
    namespace: &str,
    kind: &str,
    name: &str,
    description: &str,
    timeout: Duration,
) -> Result<()> {
    kube.wait(description, timeout, || {
        let Some(resource) = kube.optional(namespace, kind, name)? else {
            return Ok(None);
        };
        let generation = resource["metadata"]["generation"].as_i64();
        let ready = resource["status"]["conditions"]
            .as_array()
            .is_some_and(|conditions| {
                conditions.iter().any(|condition| {
                    condition["type"] == "Ready"
                        && condition["status"] == "True"
                        && generation.is_none_or(|generation| {
                            condition["observedGeneration"]
                                .as_i64()
                                .is_none_or(|observed| observed == generation)
                        })
                })
            });
        Ok(ready.then_some(()))
    })
}

fn wait_tls_secret(
    kube: &Kube<'_>,
    namespace: &str,
    name: &str,
    domain: &str,
    timeout: Duration,
) -> Result<ValidatedTls> {
    kube.wait("ingress TLS Secret", timeout, || {
        let Some(secret) = kube.optional(namespace, "secret", name)? else {
            return Ok(None);
        };
        if secret["type"] != "kubernetes.io/tls" {
            return Ok(None);
        }
        validate_tls_secret(&secret, domain).map(Some)
    })
}

fn wait_self_signed_tls_secret(
    kube: &Kube<'_>,
    namespace: &str,
    name: &str,
    domain: &str,
    timeout: Duration,
) -> Result<()> {
    kube.wait("self-signed ingress TLS Secret", timeout, || {
        let Some(secret) = kube.optional(namespace, "secret", name)? else {
            return Ok(None);
        };
        if secret["type"] != "kubernetes.io/tls" {
            return Ok(None);
        }
        validate_self_signed_tls_secret(&secret, domain).map(|()| Some(()))
    })
}

#[derive(Debug)]
struct ValidatedTls {
    roots: rustls::RootCertStore,
}

fn tls_secret_certificates(secret: &Value) -> Result<Vec<CertificateDer<'static>>> {
    let certificate = secret["data"]["tls.crt"]
        .as_str()
        .context("ingress TLS Secret has no tls.crt")?;
    let key = secret["data"]["tls.key"]
        .as_str()
        .context("ingress TLS Secret has no tls.key")?;
    let certificate = STANDARD
        .decode(certificate)
        .context("decode ingress TLS certificate")?;
    let key = STANDARD.decode(key).context("decode ingress TLS key")?;
    let certificates: Vec<_> = CertificateDer::pem_slice_iter(&certificate)
        .collect::<std::result::Result<_, _>>()
        .context("parse ingress TLS certificate chain")?;
    ensure!(
        !certificates.is_empty(),
        "ingress TLS certificate chain is empty"
    );
    let private_key =
        PrivateKeyDer::from_pem_slice(&key).context("parse ingress TLS private key")?;
    rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certificates.clone(), private_key)
        .context("ingress TLS certificate does not match its private key")?;
    Ok(certificates)
}

fn add_secret_tls_roots(secret: &Value, roots: &mut rustls::RootCertStore) -> Result<()> {
    if let Some(bundle) = secret["data"]["ca.crt"].as_str() {
        let bundle = STANDARD
            .decode(bundle)
            .context("decode ingress TLS CA bundle")?;
        if bundle.is_empty() {
            return Ok(());
        }
        let certificates: Vec<_> = CertificateDer::pem_slice_iter(&bundle)
            .collect::<std::result::Result<_, _>>()
            .context("parse ingress TLS CA bundle")?;
        ensure!(
            !certificates.is_empty(),
            "ingress TLS CA bundle has no certificates"
        );
        for certificate in certificates {
            roots
                .add(certificate)
                .context("add ingress TLS CA trust anchor")?;
        }
    }
    Ok(())
}

fn verify_tls_hostnames(
    certificates: &[CertificateDer<'static>],
    roots: rustls::RootCertStore,
    domain: &str,
    intermediates: &[CertificateDer<'static>],
) -> Result<rustls::RootCertStore> {
    let verifier = rustls::client::WebPkiServerVerifier::builder(Arc::new(roots.clone()))
        .build()
        .context("build ingress TLS certificate verifier")?;
    use rustls::client::danger::ServerCertVerifier as _;
    let mut hostnames = vec![format!("lampyr-readiness.default.{domain}")];
    // WebPKI deliberately rejects a wildcard directly below a single-label
    // name such as *.localhost. Multi-label production domains still require
    // both wildcard levels.
    if domain.contains('.') {
        hostnames.insert(0, format!("lampyr-readiness.{domain}"));
    }
    for hostname in hostnames {
        verifier
            .verify_server_cert(
                &certificates[0],
                intermediates,
                &ServerName::try_from(hostname.clone())?.to_owned(),
                &[],
                rustls::pki_types::UnixTime::now(),
            )
            .with_context(|| {
                format!(
                    "ingress TLS certificate is expired, untrusted, or does not cover {hostname}"
                )
            })?;
    }
    Ok(roots)
}

fn validate_tls_secret(secret: &Value, domain: &str) -> Result<ValidatedTls> {
    let mut roots = rustls::RootCertStore::empty();
    let native = rustls_native_certs::load_native_certs();
    let (_, ignored) = roots.add_parsable_certificates(native.certs);
    let system_empty = roots.is_empty();
    match validate_tls_secret_with_roots(secret, domain, roots) {
        Err(error) if system_empty && error.to_string().contains("TLS trust store is empty") => {
            Err(error).context(format!(
                "{ignored} system certificates were ignored while loading native roots: {:?}",
                native.errors
            ))
        }
        result => result,
    }
}

fn validate_tls_secret_with_roots(
    secret: &Value,
    domain: &str,
    mut roots: rustls::RootCertStore,
) -> Result<ValidatedTls> {
    add_secret_tls_roots(secret, &mut roots)?;
    ensure!(
        !roots.is_empty(),
        "TLS trust store is empty and the Secret has no non-empty ca.crt bundle"
    );
    let certificates = tls_secret_certificates(secret)?;
    let roots = verify_tls_hostnames(&certificates, roots, domain, &certificates[1..])?;
    Ok(ValidatedTls { roots })
}

fn validate_self_signed_tls_secret(secret: &Value, domain: &str) -> Result<()> {
    let certificates = tls_secret_certificates(secret)?;
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(certificates.last().unwrap().clone())
        .context("ingress TLS chain has no usable trust anchor")?;
    let intermediates = if certificates.len() > 1 {
        &certificates[1..certificates.len() - 1]
    } else {
        &[]
    };
    verify_tls_hostnames(&certificates, roots, domain, intermediates)?;
    Ok(())
}

fn wait_tls_endpoint(
    kube: &Kube<'_>,
    domain: &str,
    port: u16,
    tls: ValidatedTls,
    timeout: Duration,
) -> Result<()> {
    let hostname = format!("lampyr-readiness.default.{domain}");
    let server_name = ServerName::try_from(hostname.clone())?.to_owned();
    let config = Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(tls.roots)
            .with_no_client_auth(),
    );
    let mut resolver = AddressResolver::new(&hostname, port);
    kube.wait(
        &format!("trusted TLS at {hostname}:{port}"),
        timeout,
        || {
            let addresses = match resolver.resolve(kube.timeout(Duration::from_secs(2))?)? {
                Some(addresses) => addresses,
                None => return Ok(None),
            };
            for address in addresses {
                let connect_timeout = kube.timeout(Duration::from_secs(5))?;
                if let Ok(mut socket) =
                    std::net::TcpStream::connect_timeout(&address, connect_timeout)
                {
                    let io_timeout = kube.timeout(Duration::from_secs(10))?;
                    socket.set_read_timeout(Some(io_timeout))?;
                    socket.set_write_timeout(Some(io_timeout))?;
                    let mut connection =
                        rustls::ClientConnection::new(config.clone(), server_name.clone())?;
                    if connection.complete_io(&mut socket).is_ok() {
                        return Ok(Some(()));
                    }
                }
            }
            Ok(None)
        },
    )
}

fn remove_acme_resources(
    kube: &Kube<'_>,
    installation_id: &str,
    inventory: &mut Vec<Value>,
) -> Result<()> {
    let resources: Vec<_> = inventory
        .iter()
        .filter(|item| {
            item["apiVersion"] == "cert-manager.io/v1"
                && item["metadata"]["namespace"] == "kourier-system"
                && matches!(item["kind"].as_str(), Some("Certificate" | "Issuer"))
        })
        .cloned()
        .collect();
    for resource in &resources {
        let kind = resource["kind"].as_str().unwrap();
        let name = resource["metadata"]["name"].as_str().unwrap();
        let namespace = resource["metadata"]["namespace"].as_str();
        ensure_owned_tls_resource(
            kube,
            namespace,
            &kind.to_ascii_lowercase(),
            name,
            installation_id,
        )?;
        run(
            kube,
            namespace,
            &[
                "delete",
                &kind.to_ascii_lowercase(),
                name,
                "--ignore-not-found=true",
                "--wait=true",
                "--timeout=120s",
            ],
        )?;
    }
    inventory.retain(|item| !resources.contains(item));
    if !resources.is_empty() {
        persist_core_inventory(kube, installation_id, inventory.clone())?;
    }
    Ok(())
}

fn same_resource_identity(left: &Value, right: &Value) -> bool {
    left["apiVersion"] == right["apiVersion"]
        && left["kind"] == right["kind"]
        && left["metadata"]["name"] == right["metadata"]["name"]
        && left["metadata"]["namespace"] == right["metadata"]["namespace"]
}

fn remove_credential_boundary(
    kube: &Kube<'_>,
    installation_id: &str,
    previous: Option<&crate::platform_host::CertManagerCredential>,
    current: Option<&crate::platform_host::CertManagerCredential>,
    inventory: &mut Vec<Value>,
) -> Result<()> {
    let Some(previous) = previous else {
        return Ok(());
    };
    let boundary = previous.boundary["items"].as_array().unwrap();
    let current_boundary = current
        .and_then(|credential| credential.boundary["items"].as_array())
        .cloned()
        .unwrap_or_default();
    let mut resources: Vec<_> = inventory
        .iter()
        .filter(|item| {
            boundary
                .iter()
                .any(|resource| same_resource_identity(item, resource))
                && !current_boundary
                    .iter()
                    .any(|resource| same_resource_identity(item, resource))
        })
        .cloned()
        .collect();
    resources.sort_by_key(|item| usize::from(item["kind"] == "ValidatingAdmissionPolicy"));
    for resource in &resources {
        let kind = resource["kind"].as_str().unwrap().to_ascii_lowercase();
        let name = resource["metadata"]["name"].as_str().unwrap();
        ensure_owned_tls_resource(kube, None, &kind, name, installation_id)?;
        run(
            kube,
            None,
            &[
                "delete",
                &kind,
                name,
                "--ignore-not-found=true",
                "--wait=true",
                "--timeout=120s",
            ],
        )?;
    }
    inventory.retain(|item| !resources.contains(item));
    if !resources.is_empty() {
        persist_core_inventory(kube, installation_id, inventory.clone())?;
    }
    Ok(())
}

fn remove_retired_credential_boundaries(
    kube: &Kube<'_>,
    installation_id: &str,
    previous: &[crate::platform_host::CertManagerCredential],
    current: Option<&crate::platform_host::CertManagerCredential>,
    inventory: &mut Vec<Value>,
) -> Result<()> {
    for credential in previous {
        remove_credential_boundary(kube, installation_id, Some(credential), current, inventory)?;
    }
    Ok(())
}

fn remove_retired_issuers(
    kube: &Kube<'_>,
    installation_id: &str,
    current: &str,
    inventory: &mut Vec<Value>,
) -> Result<()> {
    let retired: Vec<_> = inventory
        .iter()
        .filter(|item| {
            item["apiVersion"] == "cert-manager.io/v1"
                && item["kind"] == "Issuer"
                && item["metadata"]["namespace"] == "kourier-system"
                && item["metadata"]["name"] != current
        })
        .cloned()
        .collect();
    for resource in &retired {
        let name = resource["metadata"]["name"].as_str().unwrap();
        ensure_owned_tls_resource(
            kube,
            Some("kourier-system"),
            "issuer",
            name,
            installation_id,
        )?;
        run(
            kube,
            Some("kourier-system"),
            &[
                "delete",
                "issuer",
                name,
                "--ignore-not-found=true",
                "--wait=true",
                "--timeout=120s",
            ],
        )?;
    }
    inventory.retain(|item| !retired.contains(item));
    if !retired.is_empty() {
        persist_core_inventory(kube, installation_id, inventory.clone())?;
    }
    Ok(())
}

fn self_signed_tls(kube: &Kube<'_>, domain: &str, installation_id: &str) -> Result<()> {
    crate::platform_host::validate_domain(domain)?;
    if let Some(secret) = kube.optional("kourier-system", "secret", "lampyr-kourier-tls")?
        && self_signed_secret_is_reusable(&secret, domain, installation_id)
    {
        return Ok(());
    }
    ensure_owned_tls_secret(kube, installation_id)?;
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
        "metadata": { "name": "lampyr-kourier-tls", "namespace": "kourier-system", "annotations": {
            "lampyr.dev/domain": domain, "lampyr.dev/installation-id": installation_id,
            "lampyr.dev/tls-mode":"selfSigned"
        } },
        "data": { "tls.crt": STANDARD.encode(certificate.pem()), "tls.key": STANDARD.encode(key.serialize_pem()) }
    }))
}

fn self_signed_secret_is_reusable(secret: &Value, domain: &str, installation_id: &str) -> bool {
    secret
        .pointer("/metadata/annotations/lampyr.dev~1domain")
        .and_then(Value::as_str)
        == Some(domain)
        && secret
            .pointer("/metadata/annotations/lampyr.dev~1installation-id")
            .and_then(Value::as_str)
            == Some(installation_id)
        && secret
            .pointer("/metadata/annotations/lampyr.dev~1tls-mode")
            .and_then(Value::as_str)
            == Some("selfSigned")
        && validate_self_signed_tls_secret(secret, domain).is_ok()
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
    let passes = deletion_passes(&remaining);
    let protects_provider_credential = !passes[3].is_empty();
    for (pass, items) in passes.into_iter().enumerate() {
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
        if pass == 2 && protects_provider_credential {
            wait_cert_manager_identity_gone(kube)?;
        }
    }
    Ok(namespaces)
}

fn wait_cert_manager_identity_gone(kube: &Kube<'_>) -> Result<()> {
    kube.wait(
        "cert-manager workload identity removal",
        Duration::from_secs(180),
        || {
            if kube
                .optional("default", "namespace", "cert-manager")?
                .is_none()
            {
                return Ok(Some(()));
            }
            let deployment = kube.optional("cert-manager", "deployment", "cert-manager")?;
            let pods: Vec<Value> = kube.list(
                "cert-manager",
                "pods",
                "app.kubernetes.io/name=cert-manager",
            )?;
            Ok((deployment.is_none() && pods.is_empty()).then_some(()))
        },
    )
}

/// Orders inventory deletion. Admission policies go first, since
/// fiberd-k8s-controller's refuses deleting the GrantPolicy named default.
/// Custom objects go next, so their controllers can finalize them before the
/// controllers, CRDs and namespaces. A provider credential boundary stays
/// until cert-manager is gone, so its credential cannot outlive the guard.
fn deletion_passes(remaining: &[(Value, bool)]) -> [Vec<&Value>; 4] {
    let mut passes: [Vec<&Value>; 4] = Default::default();
    for (item, custom) in remaining {
        let admission = matches!(
            item["kind"].as_str(),
            Some("ValidatingAdmissionPolicy" | "ValidatingAdmissionPolicyBinding")
        );
        let provider_credential_boundary = admission
            && item["metadata"]["annotations"]["lampyr.dev/credential-boundary"] == "true";
        passes[if provider_credential_boundary {
            3
        } else if admission {
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
    if let Some(domain) = installed.data.get("domain") {
        let tls_mode = installed
            .data
            .get("ingressTlsMode")
            .map(String::as_str)
            .unwrap_or("selfSigned");
        let secret_namespace = installed
            .data
            .get("ingressTlsSecretNamespace")
            .map(String::as_str)
            .unwrap_or("kourier-system");
        let secret_name = installed
            .data
            .get("ingressTlsSecretName")
            .map(String::as_str)
            .unwrap_or("lampyr-kourier-tls");
        let https_port = installed
            .https_port
            .as_deref()
            .unwrap_or("443")
            .parse::<u16>()
            .context("invalid installed HTTPS port")?;
        if tls_mode == "acme" {
            let issuer = installed
                .data
                .get("ingressTlsIssuer")
                .context("ACME ingress issuer is missing from installation metadata")?;
            wait_ready_resource(
                kube,
                "kourier-system",
                "issuer",
                issuer,
                "ACME issuer health",
                Duration::from_secs(10),
            )?;
            wait_ready_resource(
                kube,
                "kourier-system",
                "certificate",
                "lampyr-kourier-tls",
                "ingress Certificate health",
                Duration::from_secs(10),
            )?;
        }
        if tls_mode == "selfSigned" {
            wait_self_signed_tls_secret(
                kube,
                secret_namespace,
                secret_name,
                domain,
                Duration::from_secs(10),
            )?;
        } else {
            let tls = wait_tls_secret(
                kube,
                secret_namespace,
                secret_name,
                domain,
                Duration::from_secs(10),
            )?;
            wait_tls_endpoint(kube, domain, https_port, tls, Duration::from_secs(10))?;
        }
        if let Some(address) = installed.data.get("ingressDnsAddress") {
            let address: IpAddr = address
                .parse()
                .context("invalid installed ingress DNS address")?;
            let expected = address.to_string();
            wait_ingress_address(kube, Some(&expected), Duration::from_secs(10))?;
            wait_dns(kube, domain, address, https_port, Duration::from_secs(10))?;
        }
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
    fn bundled_manifests_load_locally_and_preserve_filtered_serving_inventory() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let mut inventory = Vec::new();
        for &name in BUNDLED_MANIFESTS {
            let source = bundled_manifest(root, name).unwrap();
            let items = if name == "serving-core.yaml" {
                let core = minimal_serving_core(&source).unwrap();
                let items = core["items"].as_array().unwrap().clone();
                assert!(items.iter().all(|item| !stock_serving_resource(item)));
                assert!(items.iter().any(|item| {
                    item["kind"] == "Deployment" && item["metadata"]["name"] == "webhook"
                }));
                items
            } else {
                manifest_items(&source).unwrap()
            };
            assert!(!items.is_empty(), "{name}");
            for item in &items {
                record_manifest(item, &mut inventory).unwrap();
            }
        }
        for name in [
            "services.serving.knative.dev",
            "certificates.cert-manager.io",
        ] {
            assert!(inventory.iter().any(|item| {
                item["kind"] == "CustomResourceDefinition" && item["metadata"]["name"] == name
            }));
        }
        assert!(!inventory.iter().any(|item| {
            item["kind"] == "Deployment" && item["metadata"]["name"] == "controller"
        }));
    }

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
        for entry in std::fs::read_dir(&k8s)
            .unwrap()
            .chain(std::fs::read_dir(k8s.join("upstream")).unwrap())
        {
            let path = entry.unwrap().path();
            if path.extension().is_none_or(|extension| extension != "yaml") {
                continue;
            }
            let source = std::fs::read_to_string(path).unwrap();
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
    fn bundled_manifests_refuse_missing_or_invalid_files_without_download_fallback() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let directory = tempfile::tempdir_in(root).unwrap();
        std::fs::create_dir_all(directory.path().join("k8s/upstream")).unwrap();
        for &name in BUNDLED_MANIFESTS {
            let path = directory.path().join("k8s/upstream").join(name);
            let error = bundled_manifest(directory.path(), name).unwrap_err();
            assert!(
                error.to_string().contains(&path.display().to_string()),
                "{error}"
            );
            assert!(
                error.to_string().contains("bundled platform manifest"),
                "{error}"
            );
            std::fs::copy(root.join("k8s/upstream").join(name), &path).unwrap();
            assert_eq!(
                bundled_manifest(directory.path(), name).unwrap(),
                bundled_manifest(root, name).unwrap()
            );
            for invalid in ["", "invalid: [yaml", "kind: Namespace\n"] {
                std::fs::write(&path, invalid).unwrap();
                let error = bundled_manifest(directory.path(), name).unwrap_err();
                assert!(error.to_string().contains("not the pinned"), "{error}");
                assert!(error.to_string().contains(name), "{error}");
            }
        }
    }

    #[test]
    fn existing_tls_secrets_must_match_the_domain_and_private_key() {
        let certified = rcgen::generate_simple_self_signed(vec![
            "*.apps.example.com".into(),
            "*.default.apps.example.com".into(),
        ])
        .unwrap();
        let secret = |key: String| {
            json!({
                "data":{
                    "tls.crt":STANDARD.encode(certified.cert.pem()),
                    "tls.key":STANDARD.encode(key),
                    "ca.crt":STANDARD.encode(certified.cert.pem())
                }
            })
        };
        validate_tls_secret_with_roots(
            &secret(certified.signing_key.serialize_pem()),
            "apps.example.com",
            rustls::RootCertStore::empty(),
        )
        .unwrap();
        let local = rcgen::generate_simple_self_signed(vec![
            "*.localhost".into(),
            "*.default.localhost".into(),
        ])
        .unwrap();
        validate_tls_secret_with_roots(
            &json!({"data":{
                "tls.crt":STANDARD.encode(local.cert.pem()),
                "tls.key":STANDARD.encode(local.signing_key.serialize_pem()),
                "ca.crt":STANDARD.encode(local.cert.pem())
            }}),
            "localhost",
            rustls::RootCertStore::empty(),
        )
        .unwrap();
        let incomplete =
            rcgen::generate_simple_self_signed(vec!["*.default.apps.example.com".into()]).unwrap();
        let incomplete_secret = json!({
            "data":{
                "tls.crt":STANDARD.encode(incomplete.cert.pem()),
                "tls.key":STANDARD.encode(incomplete.signing_key.serialize_pem()),
                "ca.crt":STANDARD.encode(incomplete.cert.pem())
            }
        });
        let error = validate_tls_secret_with_roots(
            &incomplete_secret,
            "apps.example.com",
            rustls::RootCertStore::empty(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("does not cover"), "{error}");
        assert!(
            error.contains("lampyr-readiness.apps.example.com"),
            "{error}"
        );
        assert!(
            validate_tls_secret_with_roots(
                &secret(certified.signing_key.serialize_pem()),
                "other.example.com",
                rustls::RootCertStore::empty(),
            )
            .unwrap_err()
            .to_string()
            .contains("does not cover")
        );
        let other = rcgen::KeyPair::generate().unwrap();
        assert!(
            validate_tls_secret_with_roots(
                &secret(other.serialize_pem()),
                "apps.example.com",
                rustls::RootCertStore::empty(),
            )
            .unwrap_err()
            .to_string()
            .contains("does not match")
        );
    }

    #[test]
    fn existing_tls_secret_does_not_promote_its_presented_chain_to_a_root() {
        let certified = rcgen::generate_simple_self_signed(vec![
            "*.apps.example.com".into(),
            "*.default.apps.example.com".into(),
        ])
        .unwrap();
        let secret = |ca: Option<&str>| {
            let mut data = json!({
                "tls.crt":STANDARD.encode(certified.cert.pem()),
                "tls.key":STANDARD.encode(certified.signing_key.serialize_pem())
            });
            if let Some(ca) = ca {
                data["ca.crt"] = STANDARD.encode(ca).into();
            }
            json!({"data":data})
        };
        let unrelated =
            rcgen::generate_simple_self_signed(vec!["unrelated.example.com".into()]).unwrap();
        let roots = || {
            let mut roots = rustls::RootCertStore::empty();
            roots.add(unrelated.cert.der().clone()).unwrap();
            roots
        };
        let error = validate_tls_secret_with_roots(&secret(None), "apps.example.com", roots())
            .unwrap_err()
            .to_string();
        assert!(error.contains("untrusted"), "{error}");
        let error = validate_tls_secret_with_roots(&secret(Some("")), "apps.example.com", roots())
            .unwrap_err()
            .to_string();
        assert!(error.contains("untrusted"), "{error}");
        validate_tls_secret_with_roots(
            &secret(Some(&certified.cert.pem())),
            "apps.example.com",
            roots(),
        )
        .unwrap();
    }

    #[test]
    fn existing_tls_secret_uses_intermediates_but_not_a_presented_root() {
        let mut root_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        root_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let root_key = rcgen::KeyPair::generate().unwrap();
        let root = root_params.self_signed(&root_key).unwrap();
        let root_issuer = rcgen::Issuer::new(root_params, root_key);

        let mut intermediate_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        intermediate_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let intermediate_key = rcgen::KeyPair::generate().unwrap();
        let intermediate = intermediate_params
            .signed_by(&intermediate_key, &root_issuer)
            .unwrap();
        let intermediate_issuer = rcgen::Issuer::new(intermediate_params, intermediate_key);

        let mut leaf_params = rcgen::CertificateParams::new(vec![
            "*.apps.example.com".into(),
            "*.default.apps.example.com".into(),
        ])
        .unwrap();
        leaf_params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
        let leaf_key = rcgen::KeyPair::generate().unwrap();
        let leaf = leaf_params
            .signed_by(&leaf_key, &intermediate_issuer)
            .unwrap();
        let secret = |include_ca: bool, present_root: bool| {
            let mut chain = format!("{}{}", leaf.pem(), intermediate.pem());
            if present_root {
                chain.push_str(&root.pem());
            }
            let mut data = json!({
                "tls.crt":STANDARD.encode(chain),
                "tls.key":STANDARD.encode(leaf_key.serialize_pem())
            });
            if include_ca {
                data["ca.crt"] = STANDARD.encode(root.pem()).into();
            }
            json!({"data":data})
        };
        validate_tls_secret_with_roots(
            &secret(true, false),
            "apps.example.com",
            rustls::RootCertStore::empty(),
        )
        .unwrap();
        let unrelated =
            rcgen::generate_simple_self_signed(vec!["unrelated.example.com".into()]).unwrap();
        let mut unrelated_roots = rustls::RootCertStore::empty();
        unrelated_roots.add(unrelated.cert.der().clone()).unwrap();
        let error = validate_tls_secret_with_roots(
            &secret(false, true),
            "apps.example.com",
            unrelated_roots,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("untrusted"), "{error}");
    }

    #[test]
    fn credential_reconciliation_persists_retiring_configuration() {
        let credential = |identity: &str| crate::platform_host::CertManagerCredential {
            service_account_patch: json!({"metadata":{"annotations":{"provider.example/id":identity}}}),
            service_account_cleanup_patch: json!({"metadata":{"annotations":{"provider.example/id":Value::Null}}}),
            deployment_patch: json!({"spec":{"template":{"metadata":{"annotations":{"provider.example/id":identity}}}}}),
            deployment_cleanup_patch: json!({"spec":{"template":{"metadata":{"annotations":{"provider.example/id":Value::Null}}}}}),
            controller_args: vec!["--provider-ambient=true".into()],
            controller_arg_prefixes: vec!["--provider-ambient".into()],
            boundary: json!({"apiVersion":"v1","kind":"List","items":[]}),
            boundary_message: "provider boundary".into(),
            probes: vec![],
            audits: vec![],
        };
        let first = credential("first");
        let older = credential("older");
        let second = credential("second");
        let installed = InstalledCertManagerCredentials {
            current: Some(first.clone()),
            retiring: vec![older.clone()],
        };
        assert_eq!(
            installed.previous_for(Some(&second)),
            vec![first.clone(), older.clone()]
        );
        let interrupted = InstalledCertManagerCredentials {
            current: Some(second.clone()),
            retiring: vec![first.clone(), older.clone()],
        };
        assert_eq!(
            interrupted.previous_for(Some(&second)),
            vec![first.clone(), older.clone()]
        );
        assert_eq!(
            interrupted.previous_for(None),
            vec![second.clone(), first.clone(), older.clone()]
        );
        let steady = InstalledCertManagerCredentials {
            current: Some(second.clone()),
            retiring: vec![],
        };
        assert_eq!(steady.previous_for(None), vec![second.clone()]);
        let persisted =
            cert_manager_credential_state_patch(Some(&second), &[first.clone(), older.clone()])
                .unwrap();
        let decoded = decode_installed_cert_manager_credentials(&persisted).unwrap();
        assert_eq!(decoded.current, Some(second));
        assert_eq!(decoded.retiring, vec![first, older]);
    }

    #[test]
    fn self_signed_secret_reuse_requires_the_self_signed_mode() {
        let secret = |mode: Option<&str>, expired: bool| {
            let mut annotations = json!({
                "lampyr.dev/domain":"apps.example.com",
                "lampyr.dev/installation-id":"installation"
            });
            if let Some(mode) = mode {
                annotations["lampyr.dev/tls-mode"] = mode.into();
            }
            let mut parameters = rcgen::CertificateParams::new(vec![
                "*.apps.example.com".into(),
                "*.default.apps.example.com".into(),
            ])
            .unwrap();
            parameters.not_before = rcgen::date_time_ymd(2020, 1, 1);
            parameters.not_after = if expired {
                rcgen::date_time_ymd(2021, 1, 1)
            } else {
                rcgen::date_time_ymd(2035, 1, 1)
            };
            let key = rcgen::KeyPair::generate().unwrap();
            let certificate = parameters.self_signed(&key).unwrap();
            json!({
                "metadata":{"annotations":annotations},
                "data":{
                    "tls.crt":STANDARD.encode(certificate.pem()),
                    "tls.key":STANDARD.encode(key.serialize_pem())
                }
            })
        };
        assert!(self_signed_secret_is_reusable(
            &secret(Some("selfSigned"), false),
            "apps.example.com",
            "installation"
        ));
        assert!(!self_signed_secret_is_reusable(
            &secret(Some("acme"), false),
            "apps.example.com",
            "installation"
        ));
        assert!(!self_signed_secret_is_reusable(
            &secret(None, false),
            "apps.example.com",
            "installation"
        ));
        assert!(!self_signed_secret_is_reusable(
            &secret(Some("selfSigned"), true),
            "apps.example.com",
            "installation"
        ));
    }

    #[test]
    fn cert_manager_credential_rolls_when_provider_identity_changes() {
        let deployment = json!({"spec":{"template":{"spec":{"containers":[{
                "name":"cert-manager-controller",
                "args":["--v=2","--provider-ambient=false"]
        }]}}}});
        let credential = |identity: &str| crate::platform_host::CertManagerCredential {
            service_account_patch: json!({"metadata":{"annotations":{"provider.example/id":identity}}}),
            service_account_cleanup_patch: json!({"metadata":{"annotations":{"provider.example/id":Value::Null}}}),
            deployment_patch: json!({"spec":{"template":{"metadata":{"annotations":{"provider.example/id":identity}}}}}),
            deployment_cleanup_patch: json!({"spec":{"template":{"metadata":{"annotations":{"provider.example/id":Value::Null}}}}}),
            controller_args: vec!["--provider-ambient=true".into()],
            controller_arg_prefixes: vec!["--provider-ambient".into()],
            boundary: json!({"apiVersion":"v1","kind":"List","items":[]}),
            boundary_message: "provider boundary".into(),
            probes: vec![],
            audits: vec![],
        };
        let first = credential("first");
        let patch = cert_manager_credential_patch(&deployment, Some(&first), &[]).unwrap();
        assert_eq!(
            patch.pointer("/spec/template/spec/containers/0/args"),
            Some(&json!(["--v=2", "--provider-ambient=true"]))
        );
        assert_eq!(
            patch.pointer("/spec/template/metadata/annotations/provider.example~1id"),
            Some(&json!("first"))
        );
        let second = credential("second");
        let changed =
            cert_manager_credential_patch(&deployment, Some(&second), std::slice::from_ref(&first))
                .unwrap();
        assert_ne!(
            patch.pointer("/spec/template/metadata/annotations"),
            changed.pointer("/spec/template/metadata/annotations")
        );
        let disabled =
            cert_manager_credential_patch(&patch, None, std::slice::from_ref(&second)).unwrap();
        assert_eq!(
            disabled.pointer("/spec/template/spec/containers/0/args"),
            Some(&json!(["--v=2"]))
        );
        assert_eq!(
            disabled.pointer("/spec/template/metadata/annotations/provider.example~1id"),
            Some(&Value::Null)
        );
    }

    #[test]
    fn provider_credential_boundary_is_opaque_and_auditable() {
        let boundary = json!({"apiVersion":"v1","kind":"List","items":[
            {"apiVersion":"admissionregistration.k8s.io/v1","kind":"ValidatingAdmissionPolicy",
             "metadata":{"name":"provider-boundary","annotations":{"lampyr.dev/credential-boundary":"true"}},
             "spec":{"validations":[{"expression":"!has(object.spec.providerDNS)","message":"provider boundary"}]}},
            {"apiVersion":"admissionregistration.k8s.io/v1","kind":"ValidatingAdmissionPolicyBinding",
             "metadata":{"name":"provider-boundary-binding","annotations":{"lampyr.dev/credential-boundary":"true"}},
             "spec":{"policyName":"provider-boundary","validationActions":["Deny"]}}
        ]});
        let items = boundary["items"].as_array().unwrap();
        let policy = items
            .iter()
            .find(|item| item["kind"] == "ValidatingAdmissionPolicy")
            .unwrap();
        assert_eq!(
            policy.pointer("/spec/validations/0/message"),
            Some(&json!("provider boundary"))
        );
        let binding = items
            .iter()
            .find(|item| item["kind"] == "ValidatingAdmissionPolicyBinding")
            .unwrap();
        assert_eq!(
            binding.pointer("/spec/policyName"),
            Some(&json!("provider-boundary"))
        );
        assert_eq!(
            binding.pointer("/spec/validationActions"),
            Some(&json!(["Deny"]))
        );
        assert!(field_path_exists(
            &json!({"spec":{"solvers":[{"providerDNS":{}}]}}),
            &[
                "spec".into(),
                "solvers".into(),
                "*".into(),
                "providerDNS".into()
            ]
        ));
    }

    #[test]
    fn core_ingress_lifecycle_has_no_provider_specific_credential_fields() {
        let bootstrap = include_str!("bootstrap.rs");
        let platform_host = include_str!("platform_host.rs");
        for provider_field in [
            concat!("azure", "DNS"),
            concat!("azure", ".workload.identity"),
            concat!("azure", "-pip-name"),
            concat!("azure", "-load-balancer-resource-group"),
        ] {
            assert!(!bootstrap.contains(provider_field), "{provider_field}");
        }
        for provider_field in [
            concat!("azure", "DNS"),
            concat!("azure", "-pip-name"),
            concat!("azure", "-load-balancer-resource-group"),
        ] {
            assert!(!platform_host.contains(provider_field), "{provider_field}");
        }
    }

    #[test]
    fn ingress_waits_honor_cancellation_before_network_work() {
        let cancellation = crate::runtime::Cancellation::default();
        cancellation.cancel();
        let host = crate::host::Host {
            verbose: false,
            cancellation,
        };
        let directory = tempfile::tempdir().unwrap();
        let kube = Kube::new(directory.path(), Some("test"), &host).unwrap();
        let error = wait_dns(
            &kube,
            "invalid.example",
            "203.0.113.10".parse().unwrap(),
            8443,
            Duration::from_secs(10),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("cancelled"), "{error}");
        assert!(error.contains(":8443"), "{error}");
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
        let boundary = |kind: &str, name: &str| {
            json!({ "apiVersion":"admissionregistration.k8s.io/v1",
                "kind": kind, "metadata": { "name": name, "annotations":{
                "lampyr.dev/credential-boundary":"true"
            } } })
        };
        let mut recorded = Vec::new();
        record_manifest(
            &boundary(
                "ValidatingAdmissionPolicyBinding",
                "provider-boundary-binding",
            ),
            &mut recorded,
        )
        .unwrap();
        record_manifest(
            &boundary("ValidatingAdmissionPolicy", "provider-boundary"),
            &mut recorded,
        )
        .unwrap();
        let remaining = vec![
            (item("Deployment"), false),
            (item("GrantPolicy"), true),
            (item("ValidatingAdmissionPolicyBinding"), false),
            (item("ValidatingAdmissionPolicy"), false),
            (recorded[0].clone(), false),
            (recorded[1].clone(), false),
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
                vec![
                    "ValidatingAdmissionPolicyBinding",
                    "ValidatingAdmissionPolicy"
                ],
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
