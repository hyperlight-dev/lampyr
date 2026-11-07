// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

use crate::artifact::Operation;
use crate::host::Host;
use crate::installation::Installation;
use crate::kube::Kube;

const IMAGE_PREFIX: &str = "ghcr.io/hyperlight-dev/lampyr";
// Release builds stamp their version here; development and preview builds use `preview`.
const IMAGE_TAG: &str = match option_env!("LAMPYR_IMAGE_TAG") {
    Some(tag) => tag,
    None => "preview",
};

fn default_fiberd_image() -> String {
    format!("{IMAGE_PREFIX}/fiberd:{IMAGE_TAG}")
}

fn default_router_image() -> String {
    format!("{IMAGE_PREFIX}/fiber-router:{IMAGE_TAG}")
}

pub struct Options<'a> {
    pub name: Option<&'a str>,
    pub provider: Option<&'a str>,
    pub root: Option<&'a Path>,
    pub workspace: Option<&'a Path>,
    pub artifact: Option<&'a Path>,
    pub context: Option<&'a str>,
    pub config: Value,
    pub allow_docker: bool,
    pub approve_provider_manifests: bool,
    pub interactive: bool,
    pub selection: Option<&'a Selection>,
}

#[derive(Clone, Debug)]
pub struct Selection {
    pub provider: Option<String>,
    pub name: Option<String>,
    pub context: String,
    installation_id: String,
    cluster_uid: String,
}

#[derive(Clone, Copy)]
struct ManifestApproval {
    approved: bool,
    interactive: bool,
}

struct ExistingOptions<'a> {
    name: Option<&'a str>,
    context: Option<&'a str>,
    operation: Operation,
    config: Value,
    manifests: ManifestApproval,
}

pub fn run(operation: Operation, options: Options<'_>, host: &Host) -> Result<()> {
    ensure!(
        matches!(
            operation,
            Operation::Install | Operation::Health | Operation::Uninstall
        ),
        "invalid cluster lifecycle operation"
    );
    let root = crate::service::platform_root(Path::new("."), options.root)?;
    let Some(provider) = options.provider else {
        ensure!(
            options.artifact.is_none() && !options.allow_docker,
            "provider artifact and Docker grants require --provider"
        );
        ensure!(
            options.config["deleteCluster"] != true,
            "--delete-cluster requires an infrastructure provider"
        );
        let workspace = match options.workspace {
            Some(workspace) => crate::customer_workspace::root(workspace)?,
            None => crate::customer_workspace::discover(&std::env::current_dir()?)?,
        };
        let result = existing_lifecycle(
            &root,
            &workspace,
            ExistingOptions {
                name: options.name,
                context: options.context,
                operation,
                config: options.config,
                manifests: ManifestApproval {
                    approved: options.approve_provider_manifests,
                    interactive: options.interactive,
                },
            },
            host,
        )?;
        println!("{}", serde_json::to_string(&result)?);
        return Ok(());
    };
    let name = options.name.unwrap_or("lampyr");
    let mut installation = host.verbose_step(
        &format!("provider {provider}: open installation {name} and verify artifact approval"),
        || {
            Installation::open_with_cancellation(
                &root,
                provider,
                name,
                options.workspace,
                options.artifact,
                host.cancellation.clone(),
            )
        },
    )?;
    if let Some(selection) = options.selection {
        let identity = installation.host_state.read_state("cluster-identity")?;
        validate_selection(&installation.record.id, identity.as_ref(), selection)?;
    }
    installation.authorize_docker(options.allow_docker)?;
    let mut config = options.config;
    let mapping = config
        .as_object_mut()
        .context("provider configuration must be a mapping")?;
    ensure!(
        !mapping.contains_key("name")
            && !mapping.contains_key("interactive")
            && !mapping.contains_key("context"),
        "provider configuration cannot override CLI selection or interaction policy"
    );
    mapping.insert("name".into(), name.into());
    mapping.insert("interactive".into(), options.interactive.into());
    if let Some(context) = options.context {
        mapping.insert("context".into(), context.into());
    }
    let result = lifecycle(
        &root,
        &installation,
        operation,
        &config,
        ManifestApproval {
            approved: options.approve_provider_manifests,
            interactive: options.interactive,
        },
        host,
    )?;
    println!("{}", serde_json::to_string(&result)?);
    Ok(())
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RuntimePlan {
    #[serde(default)]
    options: serde_json::Map<String, Value>,
    #[serde(default)]
    before: Vec<Value>,
    #[serde(default)]
    after: Vec<Value>,
    #[serde(default)]
    cleanup: Vec<Value>,
}

impl RuntimePlan {
    fn validate(&self) -> Result<()> {
        for manifest in self.before.iter().chain(&self.after).chain(&self.cleanup) {
            validate_manifest(manifest)?;
        }
        Ok(())
    }
}

/// RBAC objects take path segment names, and cert-manager, which Lampyr
/// installs for fiberd-k8s-controller, names some with colons.
fn rbac_name(api: &str, name: &str) -> bool {
    api.starts_with("rbac.authorization.k8s.io/")
        && !matches!(name, "" | "." | "..")
        && name.len() <= 253
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-.:".contains(&b))
}

pub(crate) fn validate_manifest(manifest: &Value) -> Result<()> {
    let api = manifest["apiVersion"].as_str().unwrap_or_default();
    let kind = manifest["kind"].as_str().unwrap_or_default();
    let name = manifest["metadata"]["name"].as_str().unwrap_or_default();
    let dns_name = |value: &str, limit: usize| {
        !value.is_empty()
            && value.len() <= limit
            && value.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && label
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                    && label.as_bytes()[0].is_ascii_alphanumeric()
                    && label.as_bytes()[label.len() - 1].is_ascii_alphanumeric()
            })
    };
    ensure!(
        !api.is_empty()
            && !api.starts_with('-')
            && api
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-./".contains(&b))
            && kind.bytes().next().is_some_and(|b| b.is_ascii_alphabetic())
            && kind.bytes().all(|b| b.is_ascii_alphanumeric())
            && (dns_name(name, 253) || rbac_name(api, name)),
        "provider returned an invalid Kubernetes manifest identity"
    );
    if let Some(namespace) = manifest["metadata"].get("namespace") {
        ensure!(
            namespace
                .as_str()
                .is_some_and(|value| dns_name(value, 63) && !value.contains('.')),
            "provider returned an invalid Kubernetes manifest namespace"
        );
    }
    Ok(())
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RuntimeRecord {
    plan: RuntimePlan,
    settings: Value,
    eventing: bool,
}

pub(crate) const RUNTIME_SECRET: &str = "lampyr-runtime";
/// The installation inputs for fiberd-k8s-controller, which only the
/// platform reads. Providers refuse configuration keys they do not know, so
/// these never reach them, and a reinstall keeps the ones it was given.
const CONTROLLER_INPUTS: [&str; 4] = [
    "controllerManifest",
    "grantImage",
    "helperImage",
    "zygoteRegistries",
];

/// Where a platform package records the controller inputs it installs when
/// an install is not given them: the release writes it beside the manifest
/// it names, with the images it published (just upstream::controller-inputs).
const CONTROLLER_DEFAULTS: &str = "build/controller-inputs.json";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ControllerDefaults {
    controller_manifest: String,
    grant_image: String,
    helper_image: String,
}

/// The controller inputs the platform at `root` installs by default, its
/// manifest resolved against `root`, or none for a platform that records
/// none, such as a checkout. zygoteRegistries is never one: it defaults
/// to the installation's own template repository (platform_host::templates).
fn controller_defaults(root: &Path) -> Result<serde_json::Map<String, Value>> {
    let path = root.join(CONTROLLER_DEFAULTS);
    let source = match std::fs::read(&path) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(serde_json::Map::new());
        }
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let defaults: ControllerDefaults =
        serde_json::from_slice(&source).with_context(|| format!("decode {}", path.display()))?;
    let manifest = Path::new(&defaults.controller_manifest);
    ensure!(
        !defaults.controller_manifest.is_empty()
            && manifest
                .components()
                .all(|part| matches!(part, std::path::Component::Normal(_))),
        "{CONTROLLER_DEFAULTS} must name a manifest inside the platform"
    );
    let manifest = root.join(manifest);
    ensure!(
        manifest.is_file(),
        "{CONTROLLER_DEFAULTS} names {}, which the platform lacks",
        manifest.display()
    );
    Ok(serde_json::Map::from_iter([
        (
            "controllerManifest".into(),
            manifest
                .to_str()
                .context("platform path must be UTF-8")?
                .into(),
        ),
        ("grantImage".into(), defaults.grant_image.into()),
        ("helperImage".into(), defaults.helper_image.into()),
    ]))
}

/// Fills each controller input `config` lacks from `defaults` and returns
/// the ones it filled. An install records only the inputs it was given, so
/// a reinstall takes the rest from the platform it runs from, and upgrading
/// the platform upgrades them.
fn fill_controller_defaults(
    config: &mut serde_json::Map<String, Value>,
    defaults: &serde_json::Map<String, Value>,
) -> Vec<String> {
    let mut filled = Vec::new();
    for (field, value) in defaults {
        if !config.contains_key(field) {
            config.insert(field.clone(), value.clone());
            filled.push(field.clone());
        }
    }
    filled
}

fn without_fields(value: &Value, fields: &[String]) -> Value {
    let mut value = value.clone();
    if let Some(mapping) = value.as_object_mut() {
        for field in fields {
            mapping.remove(field);
        }
    }
    value
}

const CREATE_ONLY_ANNOTATION: &str = "lampyr.dev/create-only";

fn validate_installation_id(identity: &str) -> Result<()> {
    ensure!(
        identity.len() == 32
            && identity
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "invalid Lampyr installation identity"
    );
    Ok(())
}

fn save_runtime(kube: &Kube<'_>, identity: &str, record: &RuntimeRecord) -> Result<()> {
    if let Some(previous) = kube.optional("kube-system", "secret", RUNTIME_SECRET)? {
        ensure!(
            previous["metadata"]["labels"]["lampyr.dev/installation-id"] == identity,
            "runtime inventory belongs to another installation"
        );
    }
    let data = serde_json::to_vec(record)?;
    ensure!(
        data.len() <= 512 * 1024,
        "runtime inventory exceeds 512 KiB"
    );
    kube.apply(&json!({"apiVersion":"v1","kind":"Secret","type":"Opaque",
        "metadata":{"name":RUNTIME_SECRET,"namespace":"kube-system",
            "labels":{"lampyr.dev/installation-id":identity}},
        "data":{"runtime":STANDARD.encode(data)}}))
}

fn read_runtime(kube: &Kube<'_>, identity: &str) -> Result<Option<RuntimeRecord>> {
    let Some(secret) = kube.optional("kube-system", "secret", RUNTIME_SECRET)? else {
        return Ok(None);
    };
    ensure!(
        secret["metadata"]["labels"]["lampyr.dev/installation-id"] == identity,
        "runtime inventory belongs to another installation"
    );
    let data = secret["data"]["runtime"]
        .as_str()
        .context("runtime inventory is missing")?;
    let record: RuntimeRecord = serde_json::from_slice(&STANDARD.decode(data)?)?;
    record.plan.validate()?;
    Ok(Some(record))
}

fn existing_lifecycle(
    root: &Path,
    workspace: &Path,
    options: ExistingOptions<'_>,
    host: &Host,
) -> Result<Value> {
    let ExistingOptions {
        name,
        context,
        operation,
        mut config,
        manifests,
    } = options;
    let kube = Kube::new(root, context, host)?;
    let previous = kube.optional("kube-system", "configmap", "lampyr-install")?;
    let data = previous.as_ref().map(|metadata| &metadata["data"]);
    let saved_name = data.and_then(|data| data["cluster"].as_str());
    if let (Some(name), Some(saved)) = (name, saved_name) {
        ensure!(
            name == saved,
            "selected context belongs to installation {saved}"
        );
    }
    let name = name.or(saved_name).unwrap_or("lampyr");
    crate::project::validate_name(name)?;
    let identity = if let Some(data) = data {
        let identity = data["installationId"]
            .as_str()
            .filter(|id| !id.is_empty())
            .context("cluster has no Lampyr installation identity")?;
        validate_installation_id(identity)?;
        identity.to_owned()
    } else {
        ensure!(
            operation == Operation::Install,
            "selected context has no Lampyr installation"
        );
        let mut identity = [0; 16];
        getrandom::fill(&mut identity)
            .map_err(|error| anyhow::anyhow!("generate installation identity: {error}"))?;
        identity.iter().map(|byte| format!("{byte:02x}")).collect()
    };
    let previous = read_runtime(&kube, &identity)?;
    if operation == Operation::Health {
        crate::bootstrap::health(&kube)?;
        if let Some(record) = previous {
            wait_manifests(&kube, &record.plan.before, "10s")?;
            wait_manifests(&kube, &record.plan.after, "10s")?;
        }
        return Ok(json!({"name":name,"healthy":true,"context":kube.context}));
    }
    if operation == Operation::Uninstall {
        crate::event_source::ensure_providerless_uninstall_safe(&kube, &identity)?;
        if let Some(record) = &previous {
            let directory = persisted_manifest_directory(&identity)?;
            approve_providerless_uninstall_manifests(directory.root(), &record.plan, manifests)?;
        }
        if let Some(record) = &previous {
            delete_manifests(&kube, &record.plan.after)?;
        }
        crate::bootstrap::uninstall_retaining_identity(&kube, workspace, &identity)?;
        if let Some(record) = previous {
            apply_manifests(&kube, &record.plan.cleanup)?;
            delete_manifests(&kube, &record.plan.before)?;
        }
        host.run(kube.command(Some("kube-system"))?.args([
            "delete",
            "secret",
            RUNTIME_SECRET,
            "--ignore-not-found=true",
        ]))?;
        crate::bootstrap::remove_installation_identity(&kube)?;
        return Ok(json!({"name":name,"uninstalled":true,"clusterRetained":true}));
    }
    let mapping = config
        .as_object_mut()
        .context("platform configuration must be a mapping")?;
    mapping.remove("deleteCluster");
    let requested_eventing = mapping.remove("eventing");
    ensure!(
        requested_eventing.as_ref().is_none_or(Value::is_boolean),
        "eventing must be a boolean"
    );
    let mut record = previous.unwrap_or(RuntimeRecord {
        plan: RuntimePlan::default(),
        settings: json!({}),
        eventing: data.is_some_and(|data| data["eventing"] == "true"),
    });
    let settings = record
        .settings
        .as_object_mut()
        .context("saved platform settings must be a mapping")?;
    settings.extend(config.as_object().unwrap().clone());
    apply_existing_cluster_defaults(settings, data)?;
    let defaulted = fill_controller_defaults(settings, &controller_defaults(root)?);
    crate::platform_host::validate_settings(&record.settings)?;
    let install_settings = record.settings.clone();
    record.settings = without_fields(&record.settings, &defaulted);
    record.eventing |= requested_eventing == Some(json!(true));
    let provider = data
        .and_then(|data| data["provider"].as_str())
        .unwrap_or("");
    let directory = persisted_manifest_directory(&identity)?;
    approve_install_manifests_at(directory.root(), &record.plan, manifests)?;
    crate::bootstrap::claim(&kube, name, provider, &identity)?;
    save_runtime(&kube, &identity, &record)?;
    apply_manifests(&kube, &record.plan.before)?;
    crate::platform_host::install(
        &kube,
        workspace,
        name,
        provider,
        &identity,
        install_settings,
    )?;
    if record.eventing {
        crate::eventing::install_core(&kube)?;
    }
    apply_manifests(&kube, &record.plan.after)?;
    crate::bootstrap::health(&kube)?;
    Ok(
        json!({"name":name,"context":kube.context,"installed":true,"ready":true,"eventing":record.eventing}),
    )
}

fn apply_existing_cluster_defaults(
    settings: &mut serde_json::Map<String, Value>,
    data: Option<&Value>,
) -> Result<()> {
    for field in ["fiberdImage", "routerImage", "domain", "httpsPort"] {
        if !settings.contains_key(field)
            && let Some(value) = data.and_then(|data| data[field].as_str())
        {
            settings.insert(
                field.into(),
                if field == "httpsPort" {
                    json!(value.parse::<u16>()?)
                } else {
                    value.into()
                },
            );
        }
    }
    settings
        .entry("fiberdImage")
        .or_insert_with(|| json!(default_fiberd_image()));
    settings
        .entry("routerImage")
        .or_insert_with(|| json!(default_router_image()));
    settings.entry("domain").or_insert(json!("localhost"));
    settings.entry("httpsPort").or_insert(json!(30443));
    settings.entry("ingressPorts").or_insert_with(|| {
        json!([
            {"name":"http2","port":80,"protocol":"TCP","targetPort":8080,"nodePort":30080},
            {"name":"https","port":443,"protocol":"TCP","targetPort":8443,"nodePort":30443},
        ])
    });
    Ok(())
}

/// Fills each controller input the install was not given from the last
/// install's, as fiberdImage and routerImage are.
fn keep_controller_inputs(config: &mut Value, previous: &Value) {
    for field in CONTROLLER_INPUTS {
        if config.get(field).is_none()
            && let Some(value) = previous.get(field)
        {
            config[field] = value.clone();
        }
    }
}

fn without_controller_inputs(config: &Value) -> Result<Value> {
    let mut provider_config = config.clone();
    let mapping = provider_config
        .as_object_mut()
        .context("provider configuration must be a mapping")?;
    for field in CONTROLLER_INPUTS {
        mapping.remove(field);
    }
    Ok(provider_config)
}

/// What the provider is given for an operation other than install: the
/// configuration without the controller inputs, which an uninstall may be
/// given in the same --provider-config as the install and the provider
/// would refuse, and marked as a teardown for an uninstall.
fn teardown_config(config: &Value, operation: Operation) -> Result<Value> {
    let mut provider_config = without_controller_inputs(config)?;
    if operation == Operation::Uninstall {
        provider_config["teardown"] = true.into();
    }
    Ok(provider_config)
}

/// Completes an install's `config` from the `previous` install's settings
/// and the platform's controller `defaults`, and checks it. It returns the
/// controller inputs it took from `defaults` too, which the install does not
/// record. It takes no provider, so an install it refuses has invoked none
/// and created no cluster.
fn install_config(
    config: &Value,
    previous: &Value,
    defaults: &serde_json::Map<String, Value>,
) -> Result<(Value, Vec<String>)> {
    let mut config = config.clone();
    for (field, default) in [
        ("fiberdImage", default_fiberd_image()),
        ("routerImage", default_router_image()),
        ("domain", "localhost".into()),
    ] {
        if config.get(field).is_none() {
            config[field] = previous.get(field).cloned().unwrap_or(default.into());
        }
        ensure!(
            config[field]
                .as_str()
                .is_some_and(|value| !value.is_empty()),
            "{field} must be nonempty text"
        );
    }
    keep_controller_inputs(&mut config, previous);
    let defaulted = fill_controller_defaults(
        config
            .as_object_mut()
            .context("provider configuration must be a mapping")?,
        defaults,
    );
    crate::platform_host::validate_install_inputs(&config)?;
    let mut images = config
        .get("images")
        .map(|images| {
            images
                .as_array()
                .cloned()
                .context("images must be an array")
        })
        .transpose()?
        .unwrap_or_default();
    images.extend([config["fiberdImage"].clone(), config["routerImage"].clone()]);
    config["images"] = images.into();
    Ok((config, defaulted))
}

/// The providers that make an installation a registry for its fiberd
/// templates: Kind and k3d run a local one, which they name as a registry
/// mirror. AKS has none whose templates grant Pods could pull without
/// credentials.
const TEMPLATE_REGISTRY_PROVIDERS: [&str; 2] = ["kind", "k3d"];

/// Refuses, before the provider makes any infrastructure, an install whose
/// provider makes no template registry and that has no zygoteRegistries.
/// The platform install checks again once the provider has named its
/// registry, if any.
fn require_template_registry(provider: &str, config: &Value) -> Result<()> {
    ensure!(
        TEMPLATE_REGISTRY_PROVIDERS.contains(&provider) || config.get("zygoteRegistries").is_some(),
        "{}",
        crate::platform_host::missing_template_registry()
    );
    Ok(())
}

fn lifecycle(
    root: &Path,
    installation: &Installation,
    operation: Operation,
    config: &Value,
    manifests: ManifestApproval,
    host: &Host,
) -> Result<Value> {
    let context = installation.platform_context(host)?;
    let platform = |operation: &str, config: Value| {
        context.invoke(
            crate::platform_host::Call {
                operation: operation.into(),
                config,
            },
            Instant::now() + Duration::from_secs(7200),
        )
    };
    let name = &installation.record.name;
    let delete = config["deleteCluster"].as_bool().unwrap_or(false);
    if operation == Operation::Install {
        let previous = installation
            .host_state
            .read_state("platform-settings")?
            .unwrap_or(json!({}));
        let (config, defaulted) = install_config(config, &previous, &controller_defaults(root)?)?;
        require_template_registry(&installation.record.provider, &config)?;
        let eventing = config["eventing"] == true || previous["eventing"] == true;
        let provider_config = without_controller_inputs(&config)?;
        installation.invoke(Operation::Init, provider_config.clone(), host)?;
        let infrastructure_existed =
            installation.invoke(Operation::Inspect, provider_config.clone(), host)?["exists"]
                .as_bool()
                .context("provider inspect result must contain boolean exists")?;
        let mut apply_provider = installation.provider(Operation::Apply, host)?;
        let provider_manifests =
            apply_provider.has_capability(crate::artifact::Capability::ProviderManifests);
        preflight_provider_manifest_approval(provider_manifests, manifests)?;
        installation.invoke_with(
            &mut apply_provider,
            Operation::Apply,
            provider_config.clone(),
            host,
        )?;
        drop(apply_provider);
        let connection = installation.invoke(Operation::Connect, provider_config.clone(), host)?;
        bind(installation, &context, &connection, true, host)?;
        let kube = Kube::new(root, connection["current-context"].as_str(), host)?;
        let plan: RuntimePlan = serde_json::from_value(
            installation
                .provider_state
                .read_state("runtime")?
                .unwrap_or(json!({})),
        )
        .context("decode provider-emitted runtime manifests")?;
        let mut settings = plan.options.clone();
        for field in ["domain", "fiberdImage", "routerImage"] {
            settings.insert(field.into(), config[field].clone());
        }
        for field in CONTROLLER_INPUTS {
            if let Some(value) = config.get(field) {
                settings.insert(field.into(), value.clone());
            }
        }
        settings.entry("httpsPort").or_insert(json!(443));
        let settings = Value::Object(settings);
        crate::platform_host::validate_settings(&settings)?;
        plan.validate()?;
        validate_provider_manifest_capability_for_plan(&plan, provider_manifests)?;
        if let Err(approval_error) = approve_install_manifests(installation, &plan, manifests) {
            if !may_rollback_rejected_install(infrastructure_existed) {
                return Err(approval_error).context(
                    "provider manifests were rejected after reconciling pre-existing infrastructure; the existing cluster was not deleted",
                );
            }
            let mut rollback_config = provider_config.clone();
            rollback_config["deleteCluster"] = true.into();
            rollback_config["teardown"] = true.into();
            let rollback = (|| -> Result<()> {
                installation.invoke(Operation::Delete, rollback_config, host)?;
                platform("connection.release", json!({}))?;
                Ok(())
            })();
            return match rollback {
                Ok(()) => Err(approval_error)
                    .context("provider infrastructure was rolled back after manifest rejection"),
                Err(rollback_error) => Err(approval_error).context(format!(
                    "provider manifests were rejected and infrastructure rollback failed: {rollback_error:#}"
                )),
            };
        }
        platform("platform.claim", json!({}))?;
        installation
            .host_state
            .write_state("runtime", &serde_json::to_value(&plan)?)?;
        installation
            .host_state
            .write_state("provider-manifests", &Value::Bool(provider_manifests))?;
        save_runtime(
            &kube,
            &installation.record.id,
            &RuntimeRecord {
                plan: plan.clone(),
                settings: without_fields(&settings, &defaulted),
                eventing,
            },
        )?;
        apply_manifests(&kube, &plan.before)?;
        platform("platform.install", settings)?;
        if eventing {
            crate::eventing::install_core(&kube)?;
        }
        apply_manifests(&kube, &plan.after)?;
        crate::bootstrap::health(&kube)?;
        let mut settings = without_fields(&config, &defaulted);
        settings["eventing"] = eventing.into();
        installation
            .host_state
            .write_state("platform-settings", &settings)?;
        return Ok(json!({"name":name,"installed":true,"ready":true,
            "provider":installation.record.provider,"context":kube.context,
            "domain":settings["domain"],"eventing":eventing}));
    }
    let teardown_config = teardown_config(config, operation)?;
    let observed = installation.invoke(Operation::Inspect, teardown_config.clone(), host)?;
    let cleanup = (|| -> Result<()> {
        if observed["exists"] == true {
            let connection =
                installation.invoke(Operation::Connect, teardown_config.clone(), host)?;
            bind(installation, &context, &connection, false, host)?;
            let kube = Kube::new(root, connection["current-context"].as_str(), host)?;
            if let Some(metadata) = kube.optional("kube-system", "configmap", "lampyr-install")? {
                ensure!(
                    metadata["data"]["installationId"] == installation.record.id,
                    "cluster belongs to another Lampyr installation"
                );
            }
            let plan: RuntimePlan = serde_json::from_value(
                installation
                    .host_state
                    .read_state("runtime")?
                    .unwrap_or(json!({})),
            )?;
            plan.validate()?;
            if operation == Operation::Health {
                ensure!(observed["ready"] == true, "infrastructure is not ready");
                crate::bootstrap::health(&kube)?;
                wait_manifests(&kube, &plan.before, "10s")?;
                wait_manifests(&kube, &plan.after, "10s")?;
                return Ok(());
            }
            let provider_manifests = installation
                .host_state
                .read_state("provider-manifests")?
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            validate_provider_manifest_capability_for_plan(&plan, provider_manifests)?;
            approve_cleanup_manifests(installation, &plan, delete, manifests)?;
            crate::event_source::uninstall_all(&kube, installation, host)?;
            delete_manifests(&kube, &plan.after)?;
            if delete {
                crate::bootstrap::remove_installation_identity(&kube)?;
            } else {
                platform("platform.uninstall", json!({}))?;
                apply_manifests(&kube, &plan.cleanup)?;
            }
            delete_manifests(&kube, &plan.before)?;
            host.run(kube.command(Some("kube-system"))?.args([
                "delete",
                "secret",
                RUNTIME_SECRET,
                "--ignore-not-found=true",
            ]))?;
        } else {
            ensure!(
                operation != Operation::Health,
                "infrastructure does not exist"
            );
        }
        Ok(())
    })();
    if operation == Operation::Health {
        cleanup?;
        return Ok(json!({"name":name,"healthy":true}));
    }
    if delete {
        installation.invoke(Operation::Delete, teardown_config.clone(), host)?;
        platform("connection.release", json!({}))?;
        cleanup.context("owned infrastructure was deleted, but Kubernetes cleanup failed; external source resources may need manual cleanup")?;
    } else {
        cleanup?;
        installation.invoke(Operation::Delete, teardown_config.clone(), host)?;
    }
    Ok(json!({"name":name,"uninstalled":true,"clusterRetained":!delete}))
}

fn validate_provider_manifest_capability_for_plan(
    plan: &RuntimePlan,
    declared: bool,
) -> Result<()> {
    let has_manifests =
        !plan.before.is_empty() || !plan.after.is_empty() || !plan.cleanup.is_empty();
    if !has_manifests {
        return Ok(());
    }
    validate_provider_manifest_capability(declared)
}

fn validate_provider_manifest_capability(declared: bool) -> Result<()> {
    ensure!(
        declared,
        "provider returned Kubernetes manifests without provider-manifests capability"
    );
    Ok(())
}

fn approve_install_manifests(
    installation: &Installation,
    plan: &RuntimePlan,
    approval: ManifestApproval,
) -> Result<()> {
    let directory = manifest_directory(installation)?;
    approve_install_manifests_at(&directory, plan, approval)
}

fn approve_install_manifests_at(
    directory: &Path,
    plan: &RuntimePlan,
    approval: ManifestApproval,
) -> Result<()> {
    if plan.before.is_empty() && plan.after.is_empty() {
        return Ok(());
    }
    let before = directory.join("before.yaml");
    let after = directory.join("after.yaml");
    write_manifest_file(&before, &plan.before)?;
    write_manifest_file(&after, &plan.after)?;
    let cleanup = directory.join("cleanup.yaml");
    if cleanup.try_exists()? {
        std::fs::remove_file(cleanup)?;
    }
    show_install_manifests(plan, &before, &after);
    require_manifest_approval(approval, "Apply these provider Kubernetes manifests?")
}

fn approve_cleanup_manifests(
    installation: &Installation,
    plan: &RuntimePlan,
    delete_cluster: bool,
    approval: ManifestApproval,
) -> Result<()> {
    let directory = manifest_directory(installation)?;
    approve_cleanup_manifests_at(&directory, plan, delete_cluster, approval)
}

fn approve_cleanup_manifests_at(
    directory: &Path,
    plan: &RuntimePlan,
    delete_cluster: bool,
    approval: ManifestApproval,
) -> Result<()> {
    if delete_cluster || plan.cleanup.is_empty() {
        return Ok(());
    }
    let cleanup = directory.join("cleanup.yaml");
    write_manifest_file(&cleanup, &plan.cleanup)?;
    show_cleanup_manifests(&plan.cleanup, &cleanup);
    require_manifest_approval(approval, "Apply the provider cleanup manifests?")
}

fn manifest_directory(installation: &Installation) -> Result<PathBuf> {
    Ok(crate::workspace::Directory::private(
        installation.host_state.root().join("provider-manifests"),
    )?
    .root()
    .to_path_buf())
}

fn persisted_manifest_directory(identity: &str) -> Result<crate::workspace::Directory> {
    validate_installation_id(identity)?;
    crate::workspace::Directory::private(
        crate::platform::home_dir()?
            .join("provider-manifests")
            .join(identity),
    )
}

fn approve_providerless_uninstall_manifests(
    directory: &Path,
    plan: &RuntimePlan,
    approval: ManifestApproval,
) -> Result<()> {
    if plan.before.is_empty() && plan.after.is_empty() && plan.cleanup.is_empty() {
        return Ok(());
    }
    let before = directory.join("before.yaml");
    let after = directory.join("after.yaml");
    let cleanup = directory.join("cleanup.yaml");
    write_manifest_file(&before, &plan.before)?;
    write_manifest_file(&after, &plan.after)?;
    write_manifest_file(&cleanup, &plan.cleanup)?;

    eprintln!();
    eprintln!("{}", style("Provider Kubernetes resource removal", "1"));
    show_manifest_phase("AFTER LAMPYR PLATFORM — DELETE", "32;1", &plan.after);
    if !plan.cleanup.is_empty() {
        show_manifest_phase("CLEANUP — APPLY", "35;1", &plan.cleanup);
    }
    show_manifest_phase(
        "BEFORE LAMPYR PLATFORM — DELETE AFTER CLEANUP",
        "36;1",
        &plan.before,
    );
    eprintln!();
    eprintln!("Files written locally:");
    eprintln!("  BEFORE  {}", style(&before.display().to_string(), "2"));
    eprintln!("  AFTER   {}", style(&after.display().to_string(), "2"));
    eprintln!("  CLEANUP {}", style(&cleanup.display().to_string(), "2"));
    eprintln!("Inspect these standard YAML files before continuing.");
    require_manifest_approval(
        approval,
        "Continue with provider Kubernetes cleanup and resource removal?",
    )
}

fn write_manifest_file(path: &Path, manifests: &[Value]) -> Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(path)?;
    for (index, manifest) in manifests.iter().enumerate() {
        if index != 0 {
            file.write_all(b"---\n")?;
        }
        file.write_all(serde_yaml_ng::to_string(manifest)?.as_bytes())?;
    }
    file.sync_all()?;
    Ok(())
}

fn show_install_manifests(plan: &RuntimePlan, before: &Path, after: &Path) {
    eprintln!();
    eprintln!("{}", style("Provider Kubernetes manifests", "1"));
    show_manifest_phase("BEFORE LAMPYR PLATFORM", "36;1", &plan.before);
    show_manifest_phase("AFTER LAMPYR PLATFORM", "32;1", &plan.after);
    eprintln!();
    eprintln!("Files written locally:");
    eprintln!("  BEFORE  {}", style(&before.display().to_string(), "2"));
    eprintln!("  AFTER   {}", style(&after.display().to_string(), "2"));
    eprintln!("Inspect these standard YAML files or apply them manually in the phase order shown.");
}

fn show_cleanup_manifests(manifests: &[Value], cleanup: &Path) {
    eprintln!();
    eprintln!("{}", style("Provider cleanup manifests", "1"));
    show_manifest_phase("CLEANUP", "35;1", manifests);
    eprintln!();
    eprintln!("File written locally:");
    eprintln!("  CLEANUP  {}", style(&cleanup.display().to_string(), "2"));
    eprintln!(
        "Inspect this standard YAML file or apply it manually during retained-cluster cleanup."
    );
}

fn show_manifest_phase(label: &str, color: &str, manifests: &[Value]) {
    eprintln!();
    eprintln!(
        "{} — {} resource{}",
        style(label, color),
        manifests.len(),
        if manifests.len() == 1 { "" } else { "s" }
    );
    for manifest in manifests {
        let kind = manifest["kind"].as_str().unwrap_or("<unknown>");
        let name = manifest["metadata"]["name"].as_str().unwrap_or("<unknown>");
        let namespace = manifest["metadata"]["namespace"].as_str();
        let identity = namespace
            .map(|namespace| format!("{namespace}/{name}"))
            .unwrap_or_else(|| name.to_owned());
        let risk = manifest_risk(manifest);
        if let Some((risk, risk_color)) = risk {
            eprintln!(
                "  {kind:<24} {identity}  {}",
                style(&format!("[{risk}]"), risk_color)
            );
        } else {
            eprintln!("  {kind:<24} {identity}");
        }
    }
}

fn manifest_risk(manifest: &Value) -> Option<(&'static str, &'static str)> {
    let pod = &manifest["spec"]["template"]["spec"];
    let privileged = ["initContainers", "containers"].iter().any(|field| {
        pod[*field].as_array().is_some_and(|containers| {
            containers
                .iter()
                .any(|container| container["securityContext"]["privileged"] == true)
        })
    });
    if privileged {
        return Some(("privileged", "31;1"));
    }
    if matches!(
        manifest["kind"].as_str(),
        Some("ClusterRole" | "ClusterRoleBinding")
    ) {
        return Some(("cluster RBAC", "33;1"));
    }
    if manifest["metadata"]["namespace"].is_null() {
        return Some(("cluster-wide", "33"));
    }
    None
}

fn require_manifest_approval(approval: ManifestApproval, prompt: &str) -> Result<()> {
    if approval.approved {
        return Ok(());
    }
    ensure!(
        approval.interactive,
        "provider Kubernetes manifests require approval; inspect the local files and rerun with --approve-provider-manifests"
    );
    eprint!("\n{prompt} [y/N] ");
    std::io::stderr().flush()?;
    let mut response = String::new();
    std::io::stdin().read_line(&mut response)?;
    ensure!(
        matches!(response.trim().to_ascii_lowercase().as_str(), "y" | "yes"),
        "provider Kubernetes manifests were not approved"
    );
    Ok(())
}

fn preflight_provider_manifest_approval(
    provider_manifests: bool,
    approval: ManifestApproval,
) -> Result<()> {
    if provider_manifests && !approval.approved && !approval.interactive {
        require_manifest_approval(approval, "Approve provider Kubernetes manifests?")
    } else {
        Ok(())
    }
}

fn may_rollback_rejected_install(infrastructure_existed: bool) -> bool {
    !infrastructure_existed
}

fn style(value: &str, code: &str) -> String {
    if std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none() {
        format!("\x1b[{code}m{value}\x1b[0m")
    } else {
        value.to_owned()
    }
}

fn bind(
    installation: &Installation,
    context: &crate::platform_host::Context,
    connection: &Value,
    select: bool,
    host: &Host,
) -> Result<()> {
    let name = connection["current-context"]
        .as_str()
        .context("provider returned no kubeconfig context")?;
    let source = serde_yaml_ng::to_string(connection)?;
    context.bind(
        &json!({"context":name,"kubeconfig":source,"select":select}),
        host,
    )?;
    ensure!(
        installation.record.id == context.installation_id,
        "connection belongs to another installation"
    );
    Ok(())
}

fn apply_manifests(kube: &Kube<'_>, manifests: &[Value]) -> Result<()> {
    for manifest in manifests {
        validate_manifest(manifest)?;
        // Bootstrap controller-owned objects without reconciling their mutable data.
        if manifest["metadata"]["annotations"][CREATE_ONLY_ANNOTATION] == "true" {
            if kube.manifest_optional(manifest)?.is_none() {
                kube.create(manifest)?;
            }
        } else {
            kube.apply(manifest)?;
        }
        if manifest["kind"] == "CustomResourceDefinition" {
            kube.wait_crds(
                &[manifest["metadata"]["name"].as_str().unwrap()],
                Duration::from_secs(120),
            )?;
        }
    }
    wait_manifests(kube, manifests, "300s")
}

fn wait_manifests(kube: &Kube<'_>, manifests: &[Value], timeout: &str) -> Result<()> {
    for manifest in manifests {
        validate_manifest(manifest)?;
        if let Some(kind @ ("Deployment" | "DaemonSet" | "StatefulSet")) = manifest["kind"].as_str()
        {
            crate::bootstrap::rollout(
                kube,
                manifest["metadata"]["namespace"]
                    .as_str()
                    .unwrap_or("default"),
                kind,
                manifest["metadata"]["name"]
                    .as_str()
                    .context("manifest has no name")?,
                timeout,
            )?;
        }
    }
    Ok(())
}

fn delete_manifests(kube: &Kube<'_>, manifests: &[Value]) -> Result<()> {
    for manifest in manifests.iter().rev() {
        validate_manifest(manifest)?;
        if manifest["kind"] == "Namespace" {
            continue;
        }
        let mut file = tempfile::NamedTempFile::new()?;
        serde_json::to_writer(file.as_file_mut(), manifest)?;
        kube.host.run(
            kube.command(manifest["metadata"]["namespace"].as_str())?
                .args(["delete", "--filename"])
                .arg(file.path())
                .args(["--ignore-not-found=true", "--wait=true", "--timeout=180s"]),
        )?;
    }
    Ok(())
}

pub fn selection(context: Option<&str>, root: Option<&Path>, host: &Host) -> Result<Selection> {
    let root = crate::service::platform_root(Path::new("."), root)?;
    let kube = Kube::new(&root, context, host)?;
    let metadata = kube
        .optional("kube-system", "configmap", "lampyr-install")?
        .context("selected context has no Lampyr installation; specify --provider and --name")?;
    let field = |name: &str| {
        metadata
            .pointer(&format!("/data/{name}"))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let provider = field("provider");
    let name = field("cluster");
    let installation_id = field("installationId")
        .context("selected Lampyr installation has no installation identity")?;
    validate_installation_id(&installation_id)?;
    ensure!(
        provider.is_none() || name.is_some(),
        "selected provider installation has no cluster name"
    );
    let namespace: Value = kube.get("default", "namespace", "kube-system")?;
    let cluster_uid = namespace["metadata"]["uid"]
        .as_str()
        .filter(|value| !value.is_empty())
        .context("selected context has no Kubernetes cluster identity")?
        .to_owned();
    Ok(Selection {
        provider,
        name,
        context: kube.context,
        installation_id,
        cluster_uid,
    })
}

fn validate_selection(
    installation_id: &str,
    identity: Option<&Value>,
    selection: &Selection,
) -> Result<()> {
    ensure!(
        installation_id == selection.installation_id,
        "selected context belongs to another Lampyr installation"
    );
    let identity =
        identity.context("selected provider installation has no recorded cluster identity")?;
    ensure!(
        identity["uid"] == selection.cluster_uid,
        "selected context belongs to a different Kubernetes cluster"
    );
    Ok(())
}

pub fn read_config(path: Option<&Path>) -> Result<Value> {
    use std::io::Read;
    let Some(path) = path else {
        return Ok(json!({}));
    };
    let mut source = Vec::new();
    std::fs::File::open(path)
        .context("open provider configuration")?
        .take(crate::runtime::PAYLOAD_LIMIT as u64 + 1)
        .read_to_end(&mut source)?;
    ensure!(
        source.len() <= crate::runtime::PAYLOAD_LIMIT,
        "provider configuration exceeds 64 KiB"
    );
    let mut config: Value =
        serde_yaml_ng::from_slice(&source).context("decode provider configuration")?;
    ensure!(
        config.is_object(),
        "provider configuration must be a mapping"
    );
    // The install records the manifest's path and a reinstall reuses it
    // from wherever it runs, so a relative one is the configuration's own.
    if let Some(manifest) = config["controllerManifest"].as_str()
        && !manifest.is_empty()
        && !manifest.contains("://")
        && Path::new(manifest).is_relative()
    {
        let base = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let manifest = std::path::absolute(base.join(manifest))
            .context("resolve controllerManifest against the provider configuration")?;
        config["controllerManifest"] = manifest
            .to_str()
            .context("controllerManifest path must be UTF-8")?
            .into();
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform_host::tests::{grant_image, helper_image, manifest};

    #[test]
    fn a_relative_controller_manifest_is_the_provider_configuration_s() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("inputs.yaml");
        let read = |manifest: &str| {
            std::fs::write(&path, format!("controllerManifest: {manifest}\n")).unwrap();
            read_config(Some(&path)).unwrap()["controllerManifest"].clone()
        };
        let expected = std::path::absolute(directory.path().join("build/controller.yaml")).unwrap();
        assert_eq!(read("build/controller.yaml"), expected.to_str().unwrap());
        assert_eq!(read("/opt/controller.yaml"), "/opt/controller.yaml");
        assert_eq!(
            read("https://example.com/controller.yaml"),
            "https://example.com/controller.yaml"
        );
        std::fs::write(&path, "grantImage: registry.example/fiberd@sha256:00\n").unwrap();
        assert!(
            read_config(Some(&path))
                .unwrap()
                .get("controllerManifest")
                .is_none()
        );
    }

    #[test]
    fn rbac_manifests_take_path_segment_names() {
        let manifest = |api: &str, kind: &str, name: &str| json!({ "apiVersion": api, "kind": kind, "metadata": { "name": name } });
        for (api, kind, name, valid) in [
            (
                "rbac.authorization.k8s.io/v1",
                "Role",
                "cert-manager-cainjector:leaderelection",
                true,
            ),
            (
                "rbac.authorization.k8s.io/v1",
                "ClusterRole",
                "cert-manager-webhook:subjectaccessreviews",
                true,
            ),
            ("v1", "ConfigMap", "cert-manager:leaderelection", false),
            ("rbac.authorization.k8s.io/v1", "ClusterRole", ".", false),
            ("rbac.authorization.k8s.io/v1", "ClusterRole", "..", false),
        ] {
            assert_eq!(
                validate_manifest(&manifest(api, kind, name)).is_ok(),
                valid,
                "{kind} {name}"
            );
        }
    }

    #[test]
    fn noninteractive_provider_manifests_require_the_explicit_flag() {
        assert!(
            require_manifest_approval(
                ManifestApproval {
                    approved: true,
                    interactive: false,
                },
                "ignored",
            )
            .is_ok()
        );
        assert!(
            require_manifest_approval(
                ManifestApproval {
                    approved: false,
                    interactive: false,
                },
                "ignored",
            )
            .unwrap_err()
            .to_string()
            .contains("--approve-provider-manifests")
        );
        assert!(
            preflight_provider_manifest_approval(
                true,
                ManifestApproval {
                    approved: false,
                    interactive: false,
                },
            )
            .unwrap_err()
            .to_string()
            .contains("--approve-provider-manifests")
        );
        assert!(
            preflight_provider_manifest_approval(
                false,
                ManifestApproval {
                    approved: false,
                    interactive: false,
                },
            )
            .is_ok()
        );
    }

    #[test]
    fn manifest_rejection_never_authorizes_deleting_preexisting_infrastructure() {
        assert!(may_rollback_rejected_install(false));
        assert!(!may_rollback_rejected_install(true));
    }

    #[test]
    fn existing_clusters_default_to_preloaded_platform_images() {
        let mut settings = serde_json::Map::new();
        apply_existing_cluster_defaults(&mut settings, None).unwrap();
        assert_eq!(settings["fiberdImage"], default_fiberd_image());
        assert_eq!(settings["routerImage"], default_router_image());
        assert_eq!(settings["domain"], "localhost");
        assert_eq!(settings["httpsPort"], 30443);
        assert_eq!(settings["ingressPorts"][0]["nodePort"], 30080);
        assert_eq!(settings["ingressPorts"][1]["nodePort"], 30443);

        let mut settings =
            serde_json::Map::from_iter([("routerImage".into(), json!("custom/router:v1"))]);
        let installed = json!({
            "fiberdImage": "custom/fiberd:v1",
            "routerImage": "saved/router:v1",
            "domain": "services.example.com",
            "httpsPort": "8443",
        });
        apply_existing_cluster_defaults(&mut settings, Some(&installed)).unwrap();
        assert_eq!(settings["fiberdImage"], "custom/fiberd:v1");
        assert_eq!(settings["routerImage"], "custom/router:v1");
        assert_eq!(settings["domain"], "services.example.com");
        assert_eq!(settings["httpsPort"], 8443);
    }

    #[test]
    fn controller_inputs_stay_with_the_platform_across_reinstalls() {
        let given = json!({
            "fiberdImage": "custom/fiberd:v1",
            "controllerManifest": "/controller.yaml",
            "grantImage": "registry/fiberd@sha256:1",
            "helperImage": "registry/helper@sha256:2",
            "zygoteRegistries": ["http://registry:5000/ci"],
            "registryPort": 5002,
        });
        assert_eq!(
            without_controller_inputs(&given).unwrap(),
            json!({"fiberdImage": "custom/fiberd:v1", "registryPort": 5002})
        );

        let mut reinstall = json!({"grantImage": "registry/fiberd@sha256:3"});
        keep_controller_inputs(&mut reinstall, &given);
        assert_eq!(reinstall["grantImage"], "registry/fiberd@sha256:3");
        assert_eq!(reinstall["controllerManifest"], "/controller.yaml");
        assert_eq!(reinstall["helperImage"], "registry/helper@sha256:2");
        assert_eq!(
            reinstall["zygoteRegistries"],
            json!(["http://registry:5000/ci"])
        );
        assert!(reinstall.get("registryPort").is_none());

        let mut first = json!({});
        keep_controller_inputs(&mut first, &json!({}));
        assert_eq!(first, json!({}));
    }

    #[test]
    fn an_uninstall_given_the_install_s_config_passes_no_controller_input_on() {
        let given = json!({
            "name": "lampyr",
            "deleteCluster": true,
            "controllerManifest": "/controller.yaml",
            "grantImage": "registry/fiberd@sha256:1",
            "helperImage": "registry/helper@sha256:2",
            "zygoteRegistries": ["http://registry:5000/ci"],
        });
        assert_eq!(
            teardown_config(&given, Operation::Uninstall).unwrap(),
            json!({"name": "lampyr", "deleteCluster": true, "teardown": true})
        );
        assert_eq!(
            teardown_config(&given, Operation::Health).unwrap(),
            json!({"name": "lampyr", "deleteCluster": true})
        );
    }

    #[test]
    fn a_first_install_needs_every_controller_input_before_the_provider_runs() {
        let given = json!({
            "controllerManifest": manifest(),
            "grantImage": grant_image(),
            "helperImage": helper_image(),
            "zygoteRegistries": ["http://registry:5000/ci"],
        });
        let none = serde_json::Map::new();
        let (config, defaulted) = install_config(&given, &json!({}), &none).unwrap();
        assert_eq!(config["controllerManifest"], manifest());
        assert_eq!(
            config["images"],
            json!([default_fiberd_image(), default_router_image()])
        );
        assert!(defaulted.is_empty());
        for field in CONTROLLER_INPUTS {
            let mut missing = given.clone();
            missing.as_object_mut().unwrap().remove(field);
            // zygoteRegistries defaults to the installation's template
            // repository, which only the provider knows.
            if field == "zygoteRegistries" {
                install_config(&missing, &json!({}), &none).unwrap();
                continue;
            }
            let error = install_config(&missing, &json!({}), &none)
                .unwrap_err()
                .to_string();
            assert!(error.contains(field), "{field}: {error}");
            // A reinstall keeps the input the last install was given.
            install_config(&missing, &given, &none).unwrap();
        }
    }

    #[test]
    fn an_install_whose_provider_makes_no_template_registry_needs_zygote_registries() {
        let given = json!({
            "controllerManifest": manifest(),
            "grantImage": grant_image(),
            "helperImage": helper_image(),
            "zygoteRegistries": ["ghcr.io/team/templates"],
        });
        for provider in ["kind", "k3d"] {
            require_template_registry(provider, &json!({})).unwrap();
        }
        require_template_registry("azure", &given).unwrap();
        let error = require_template_registry("azure", &json!({}))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("zygoteRegistries") && error.contains("anonymous"),
            "{error}"
        );
        // A reinstall keeps the zygoteRegistries the last install was given.
        let none = serde_json::Map::new();
        let (config, _) = install_config(&json!({}), &given, &none).unwrap();
        require_template_registry("azure", &config).unwrap();
    }

    #[test]
    fn a_platform_s_controller_defaults_fill_only_the_inputs_an_install_lacks() {
        let digest = |byte: &str| format!("sha256:{}", byte.repeat(64));
        let defaults = serde_json::Map::from_iter([
            ("controllerManifest".into(), json!(manifest())),
            (
                "grantImage".into(),
                json!(format!("ghcr.io/team/fiberd@{}", digest("1"))),
            ),
            (
                "helperImage".into(),
                json!(format!("ghcr.io/team/helper@{}", digest("2"))),
            ),
        ]);
        let given = json!({
            "grantImage": format!("registry.example/fiberd@{}", digest("3")),
            "zygoteRegistries": ["http://registry:5000/ci"],
        });
        let previous = json!({"helperImage": format!("registry.example/helper@{}", digest("4"))});
        let (config, defaulted) = install_config(&given, &previous, &defaults).unwrap();
        assert_eq!(config["grantImage"], given["grantImage"]);
        assert_eq!(config["helperImage"], previous["helperImage"]);
        assert_eq!(config["controllerManifest"], manifest());
        assert_eq!(defaulted, ["controllerManifest"]);
        // The install records what it was given, not the platform's defaults.
        assert!(
            without_fields(&config, &defaulted)
                .get("controllerManifest")
                .is_none()
        );
        // The platform has no default zygoteRegistries; the installation's
        // template repository is its default.
        let (config, _) = install_config(&json!({}), &json!({}), &defaults).unwrap();
        assert!(config.get("zygoteRegistries").is_none());
    }

    #[test]
    fn a_platform_records_controller_defaults_beside_the_manifest_it_names() {
        let root = tempfile::tempdir().unwrap();
        assert!(controller_defaults(root.path()).unwrap().is_empty());
        std::fs::create_dir(root.path().join("build")).unwrap();
        let write = |manifest: &str| {
            std::fs::write(
                root.path().join(CONTROLLER_DEFAULTS),
                json!({
                    "controllerManifest": manifest,
                    "grantImage": "ghcr.io/team/fiberd@sha256:1",
                    "helperImage": "ghcr.io/team/helper@sha256:2",
                })
                .to_string(),
            )
            .unwrap();
        };
        write("build/fiberd-k8s-controller.yaml");
        let error = controller_defaults(root.path()).unwrap_err().to_string();
        assert!(error.contains("the platform lacks"), "{error}");
        std::fs::write(root.path().join("build/fiberd-k8s-controller.yaml"), "").unwrap();
        let defaults = controller_defaults(root.path()).unwrap();
        assert_eq!(
            defaults["controllerManifest"],
            root.path()
                .join("build/fiberd-k8s-controller.yaml")
                .to_str()
                .unwrap()
        );
        assert_eq!(defaults["grantImage"], "ghcr.io/team/fiberd@sha256:1");
        assert_eq!(defaults["helperImage"], "ghcr.io/team/helper@sha256:2");
        for outside in ["../controller.yaml", "/controller.yaml", ""] {
            write(outside);
            let error = controller_defaults(root.path()).unwrap_err().to_string();
            assert!(error.contains("inside the platform"), "{outside}: {error}");
        }
    }

    #[test]
    fn manifest_files_are_multi_document_yaml_and_risks_are_classified() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("before.yaml");
        let manifests = vec![
            json!({"apiVersion":"v1","kind":"ConfigMap",
                "metadata":{"name":"settings","namespace":"default"}}),
            json!({"apiVersion":"apps/v1","kind":"DaemonSet",
            "metadata":{"name":"node-setup","namespace":"kube-system"},
            "spec":{"template":{"spec":{"initContainers":[{
                "name":"setup","securityContext":{"privileged":true}
            }]}}}}),
        ];
        write_manifest_file(&path, &manifests).unwrap();
        let source = std::fs::read_to_string(path).unwrap();
        assert_eq!(source.matches("---\n").count(), 1);
        assert_eq!(manifest_risk(&manifests[0]), None);
        assert_eq!(manifest_risk(&manifests[1]), Some(("privileged", "31;1")));
    }

    #[test]
    fn provider_plans_require_the_declared_capability() {
        assert!(validate_provider_manifest_capability(true).is_ok());
        assert!(
            validate_provider_manifest_capability(false)
                .unwrap_err()
                .to_string()
                .contains("provider-manifests capability")
        );
    }

    #[test]
    fn selected_context_must_match_local_installation_and_cluster_identity() {
        let selection = Selection {
            provider: Some("azure".into()),
            name: Some("example".into()),
            context: "customer".into(),
            installation_id: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            cluster_uid: "cluster-a".into(),
        };
        let identity = json!({"uid":"cluster-a","context":"installed"});
        validate_selection(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            Some(&identity),
            &selection,
        )
        .unwrap();
        assert!(
            validate_selection(
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                Some(&identity),
                &selection,
            )
            .unwrap_err()
            .to_string()
            .contains("another Lampyr installation")
        );
        assert!(
            validate_selection(
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                Some(&json!({"uid":"cluster-b"})),
                &selection,
            )
            .unwrap_err()
            .to_string()
            .contains("different Kubernetes cluster")
        );
    }

    #[test]
    fn installation_identity_must_be_lowercase_hex_without_path_components() {
        assert!(validate_installation_id(&"a".repeat(32)).is_ok());
        for identity in [
            "../escape",
            "Aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "gggggggggggggggggggggggggggggggg",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ] {
            assert!(validate_installation_id(identity).is_err(), "{identity}");
        }
    }

    #[test]
    fn providerless_replay_writes_and_approves_every_manifest_phase() {
        let manifest = |name: &str| {
            json!({"apiVersion":"v1","kind":"ConfigMap",
                "metadata":{"name":name,"namespace":"default"}})
        };
        let plan = RuntimePlan {
            before: vec![manifest("before")],
            after: vec![manifest("after")],
            cleanup: vec![manifest("cleanup")],
            ..Default::default()
        };
        let directory = tempfile::tempdir().unwrap();
        approve_install_manifests_at(
            directory.path(),
            &plan,
            ManifestApproval {
                approved: true,
                interactive: false,
            },
        )
        .unwrap();
        approve_providerless_uninstall_manifests(
            directory.path(),
            &plan,
            ManifestApproval {
                approved: true,
                interactive: false,
            },
        )
        .unwrap();
        for phase in ["before.yaml", "after.yaml", "cleanup.yaml"] {
            assert!(directory.path().join(phase).is_file(), "{phase}");
        }

        assert!(
            approve_providerless_uninstall_manifests(
                directory.path(),
                &plan,
                ManifestApproval {
                    approved: false,
                    interactive: false,
                },
            )
            .unwrap_err()
            .to_string()
            .contains("--approve-provider-manifests")
        );
    }
}
