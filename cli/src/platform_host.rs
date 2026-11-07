// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context as _, Result, bail, ensure};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::host::Host;
use crate::kube::Kube;
use crate::workspace::Directory;

pub(crate) struct Context {
    pub root: PathBuf,
    pub workspace: PathBuf,
    pub state: Arc<Directory>,
    pub installation_id: String,
    pub provider: String,
    pub name: String,
    pub host: Host,
}

pub(crate) struct Call {
    pub operation: String,
    pub config: Value,
}

impl Context {
    pub(crate) fn invoke(&self, call: Call, deadline: Instant) -> Result<Value> {
        let mut host = self.host.clone();
        host.cancellation = host.cancellation.with_deadline(deadline);
        ensure!(
            !host.cancellation.is_cancelled(),
            "platform operation cancelled or past its deadline"
        );
        if call.operation == "connection.release" {
            if let Some(mut identity) = self.state.read_state("cluster-identity")? {
                let context = identity["context"]
                    .as_str()
                    .context("installation connection has no context")?;
                crate::connection::release(
                    &crate::connection::config_path()?,
                    context,
                    &self.installation_id,
                )?;
                identity["released"] = true.into();
                self.state.write_state("cluster-identity", &identity)?;
            }
            return Ok(json!({}));
        }
        let identity = self
            .state
            .read_state("cluster-identity")?
            .context("installation has no Kubernetes connection")?;
        let context = identity["context"]
            .as_str()
            .context("installation connection has no context")?;
        let uid = identity["uid"]
            .as_str()
            .context("installation connection has no cluster identity")?;
        let kube = Kube::new(&self.root, Some(context), &host)?;
        let namespace: Value = kube.get("default", "namespace", "kube-system")?;
        ensure!(
            namespace["metadata"]["uid"] == uid,
            "Kubernetes cluster has been replaced outside this installation"
        );
        match call.operation.as_str() {
            "platform.claim" => {
                crate::bootstrap::claim(&kube, &self.name, &self.provider, &self.installation_id)?
            }
            "platform.install" => {
                install(
                    &kube,
                    &self.workspace,
                    &self.name,
                    &self.provider,
                    &self.installation_id,
                    call.config,
                )?;
            }
            "platform.uninstall" => {
                if kube
                    .optional("kube-system", "configmap", "lampyr-install")?
                    .is_some()
                {
                    crate::bootstrap::uninstall(&kube, &self.workspace, &self.installation_id)?;
                }
            }
            operation => bail!("unsupported platform operation: {operation}"),
        }
        Ok(json!({}))
    }

    pub(crate) fn bind(&self, config: &Value, host: &Host) -> Result<Value> {
        let source = config["kubeconfig"]
            .as_str()
            .context("kubeconfig must be text")?;
        let context = config["context"].as_str().context("context must be text")?;
        crate::connection::validate(source, context)?;
        let mut private = tempfile::NamedTempFile::new()?;
        private.write_all(source.as_bytes())?;
        let namespace = host.capture(
            host.command("kubectl")?
                .arg("--kubeconfig")
                .arg(private.path())
                .args([
                    "--context",
                    context,
                    "--request-timeout=15s",
                    "get",
                    "namespace",
                    "kube-system",
                    "--output",
                    "json",
                ]),
        )?;
        let namespace: Value = serde_json::from_str(&namespace)?;
        let uid = namespace["metadata"]["uid"]
            .as_str()
            .filter(|value| !value.is_empty())
            .context("provider kubeconfig has no Kubernetes cluster identity")?;
        if let Some(previous) = self.state.read_state("cluster-identity")? {
            ensure!(
                previous["released"] == true || previous["uid"] == uid,
                "provider kubeconfig targets a replaced Kubernetes cluster"
            );
        }
        crate::connection::persist(
            &crate::connection::config_path()?,
            source,
            &crate::connection::Profile {
                context,
                installation_id: &self.installation_id,
                select: config["select"].as_bool().unwrap_or(false),
            },
        )?;
        self.state.write_state(
            "cluster-identity",
            &json!({"uid":uid,"context":context,"released":false}),
        )?;
        Ok(json!({"context":context,"uid":uid}))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PlatformSettings {
    domain: String,
    https_port: u16,
    fiberd_image: String,
    router_image: String,
    controller_manifest: String,
    grant_image: String,
    helper_image: String,
    #[serde(default)]
    zygote_registries: Vec<String>,
    #[serde(default)]
    registry_mirrors: BTreeMap<String, Value>,
    #[serde(default)]
    ingress_ports: Option<Value>,
    #[serde(default)]
    metadata: BTreeMap<String, String>,
    #[serde(default)]
    home_pod_metadata: Option<Value>,
}

impl PlatformSettings {
    fn validate(&self) -> Result<()> {
        validate_domain(&self.domain)?;
        ensure!(self.https_port > 0, "HTTPS port must be positive");
        for (field, image) in [
            ("fiberdImage", &self.fiberd_image),
            ("routerImage", &self.router_image),
        ] {
            validate_image(field, image)?;
        }
        validate_controller_inputs(
            &self.controller_manifest,
            &self.grant_image,
            &self.helper_image,
        )?;
        if let Some(ports) = &self.ingress_ports {
            ensure!(ports.is_array(), "ingress ports must be an array");
        }
        for reserved in [
            "installationId",
            "cluster",
            "provider",
            crate::bootstrap::CORE_INVENTORY_KEY,
        ] {
            ensure!(
                !self.metadata.contains_key(reserved),
                "provider platform metadata cannot replace {reserved}"
            );
        }
        if let Some(metadata) = &self.home_pod_metadata {
            validate_metadata(metadata)?;
        }
        self.templates().map(|_| ())
    }

    /// Where ly deploy pushes fiberd templates, and the zygoteRegistries
    /// the GrantPolicy lists for them.
    fn templates(&self) -> Result<Templates> {
        templates(&self.zygote_registries, &self.registry_mirrors)
    }
}

/// An installation's one fiberd template repository, as the host pushes
/// to it, the repositories grant Pods may pull templates from, and the
/// registry mirrors the serving controller rewrites templates through.
#[derive(Debug, PartialEq)]
pub(crate) struct Templates {
    pub repository: String,
    pub registries: Vec<String>,
    pub mirrors: BTreeMap<String, Value>,
}

/// The repository, under the installation's registry, that ly deploy
/// pushes every fiberd template to. Templates are referenced by digest,
/// so its tags, one per Revision, are only for people and garbage
/// collection.
pub(crate) const TEMPLATE_REPOSITORY: &str = "lampyr/fiberd-templates";

/// Resolves an installation's templates. ly deploy pushes every template
/// to one repository, which it picks this way:
///
/// - If zygoteRegistries is given and admits a local registry mirror's
///   in-cluster target, the repository is TEMPLATE_REPOSITORY in that
///   mirror: the host pushes through the mirror's key and grant Pods pull
///   from its target. This is Kind and k3d's normal case.
/// - Otherwise, if zygoteRegistries is given, it is the first entry. If
///   that entry's registry is a local mirror's in-cluster target, the host
///   pushes through that mirror's key at the given path, since it may not
///   reach the target's name. Otherwise it pushes to the entry directly.
/// - Without zygoteRegistries, a mirror is the template registry only if
///   it is a local registry.
///
/// A local registry mirror is keyed localhost, 127.0.0.1 or ::1, or is
/// reached over plain HTTP and keyed by no public registry, since a
/// pull-through cache of a public registry has nothing to do with
/// templates and pushing through its key would push to what it caches.
/// Without exactly one local mirror and no zygoteRegistries, the
/// installation must give zygoteRegistries.
///
/// Grant Pods pull templates without credentials, so the repository must
/// allow anonymous pulls; what they pull is sealed to the fleet key.
/// zygoteRegistries defaults to the repository spelled as grant Pods reach
/// it, and a given list must admit that spelling.
pub(crate) fn templates(given: &[String], mirrors: &BTreeMap<String, Value>) -> Result<Templates> {
    let given = zygote_registries(given)?;
    // fiberd-k8s-controller compares schemes too, so plain HTTP passes
    // only under an entry that spells http://.
    let admitted = |pulled: &str| {
        given.iter().any(|registry| {
            pulled == registry
                || pulled
                    .strip_prefix(registry.as_str())
                    .is_some_and(|rest| rest.starts_with('/'))
        })
    };
    let repository = match given.first() {
        None => {
            let local: Vec<&str> = mirrors
                .iter()
                .filter(|(registry, mirror)| local_registry(registry, mirror))
                .map(|(registry, _)| registry.as_str())
                .collect();
            match local.as_slice() {
                [registry] => pushed_through(registry, mirrors, TEMPLATE_REPOSITORY),
                _ => bail!("{}", missing_template_registry()),
            }
        }
        Some(first) => {
            let mut registries = Vec::new();
            // Pushing through a pull-through cache's key would push to the
            // registry it caches, such as Docker Hub.
            for registry in mirrors
                .iter()
                .filter(|(registry, mirror)| local_registry(registry, mirror))
                .map(|(registry, _)| registry)
            {
                let repository = format!("{registry}/{TEMPLATE_REPOSITORY}");
                if admitted(&mirrored(&repository, mirrors)?) {
                    registries.push(registry.as_str());
                }
            }
            match registries.as_slice() {
                [registry] => pushed_through(registry, mirrors, TEMPLATE_REPOSITORY),
                [] => through_mirror(first, mirrors)?,
                registries => bail!(
                    "the installation has several registries ({}), so Lampyr cannot pick the \
                     one fiberd templates go to",
                    registries.join(", ")
                ),
            }
        }
    };
    let mut mirrors = mirrors.clone();
    let (plain_http, bare) = match repository.strip_prefix("http://") {
        Some(bare) => (true, bare.to_owned()),
        None => (false, repository.clone()),
    };
    crate::registry::validate_tag(&format!("{bare}:template"))
        .with_context(|| format!("invalid fiberd template repository {repository:?}"))?;
    // validate_tag accepts empty path components, which make no reference
    // ly deploy could push to, so `ghcr.io/team/` is no repository.
    let (registry, _) = bare
        .split_once('/')
        .filter(|(registry, path)| {
            !registry.is_empty() && path.split('/').all(|component| !component.is_empty())
        })
        .context("a template repository needs a registry and a path")?;
    // A template reference carries no scheme, so grant Pods reach a plain
    // HTTP repository through a mirror of its registry to itself.
    if plain_http && !mirrors.contains_key(registry) {
        mirrors.insert(
            registry.to_owned(),
            json!({"registry": registry, "plainHttp": true}),
        );
    }
    let pulled = mirrored(&bare, &mirrors)?;
    if given.is_empty() {
        // The provider's mirrors decide the default, so it is checked
        // against the GrantPolicy's schema only now.
        zygote_registries(std::slice::from_ref(&pulled))
            .context("the installation's template repository makes no zygoteRegistries entry")?;
        return Ok(Templates {
            repository,
            registries: vec![pulled],
            mirrors,
        });
    }
    ensure!(
        admitted(&pulled),
        "zygoteRegistries must admit {pulled}, where grant Pods pull the templates \
         ly deploy pushes to {repository}"
    );
    Ok(Templates {
        repository,
        registries: given,
        mirrors,
    })
}

/// The GrantPolicy's limits on spec.registries. The API server enforces
/// them only when the platform install applies the policy, after the
/// provider has made the cluster and the installation has claimed it, so
/// ly checks every entry first.
const MAX_ZYGOTE_REGISTRIES: usize = 64;
const MAX_ZYGOTE_REGISTRY_LENGTH: usize = 255;

/// `given` as the GrantPolicy lists it, each entry checked against the
/// CRD's registries schema: fiberd-k8s-controller's RegistryPattern, its
/// item length and count, and its set semantics.
fn zygote_registries(given: &[String]) -> Result<Vec<String>> {
    ensure!(
        given.len() <= MAX_ZYGOTE_REGISTRIES,
        "zygoteRegistries lists {} repositories; the GrantPolicy allows at most \
         {MAX_ZYGOTE_REGISTRIES}",
        given.len()
    );
    let mut registries: Vec<String> = Vec::with_capacity(given.len());
    for entry in given {
        // TLS is the default, and the GrantPolicy spells only plain HTTP.
        let registry = entry.strip_prefix("https://").unwrap_or(entry);
        // Stripped, https://http:// would pass for a plain HTTP entry.
        ensure!(
            registry.len() == entry.len() || !registry.starts_with("http://"),
            "invalid zygoteRegistries entry {entry:?}: give one scheme, http:// or https://"
        );
        ensure!(
            registry.len() <= MAX_ZYGOTE_REGISTRY_LENGTH && registry_pattern(registry),
            "invalid zygoteRegistries entry {entry:?}: give a repository such as \
             myacr.azurecr.io/templates or http://registry.local:5000/templates, whose \
             registry is localhost or has a dot or a port, with a lowercase path and no \
             tag or digest, in at most {MAX_ZYGOTE_REGISTRY_LENGTH} characters"
        );
        ensure!(
            !registries.iter().any(|listed| listed == registry),
            "zygoteRegistries lists {registry:?} twice"
        );
        registries.push(registry.to_owned());
    }
    Ok(registries)
}

/// fiberd-k8s-controller's RegistryPattern (api/v1alpha1/constants.go):
/// an optional http:// and a repository whose host is localhost, a name
/// with a dot or a name with a port, and one or more OCI path components.
/// A bare name such as `myteam/templates` would otherwise pass for a
/// Docker Hub repository.
pub(crate) fn registry_pattern(registry: &str) -> bool {
    let bare = registry.strip_prefix("http://").unwrap_or(registry);
    let Some((host, path)) = bare.split_once('/') else {
        return false;
    };
    let (name, port) = match host.split_once(':') {
        Some((name, port)) => (name, Some(port)),
        None => (host, None),
    };
    let label = |label: &str| {
        let bytes = label.as_bytes();
        !bytes.is_empty()
            && bytes
                .iter()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
            && bytes[0] != b'-'
            && bytes[bytes.len() - 1] != b'-'
    };
    let port_ok = port.is_none_or(|port| {
        (1..=5).contains(&port.len()) && port.bytes().all(|byte| byte.is_ascii_digit())
    });
    port_ok
        && name.split('.').all(label)
        && (name == "localhost" || name.contains('.') || port.is_some())
        && path.split('/').all(path_component)
}

/// An OCI path component: lowercase alphanumeric runs joined by `.`, `_`,
/// `__` or any number of `-`.
fn path_component(component: &str) -> bool {
    let alnum = |byte: &u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    let bytes = component.as_bytes();
    let mut index = 0;
    loop {
        let start = index;
        index += bytes[index..].iter().take_while(|byte| alnum(byte)).count();
        if index == start {
            return false;
        }
        if index == bytes.len() {
            return true;
        }
        let separator = index;
        index += bytes[index..]
            .iter()
            .take_while(|byte| !alnum(byte))
            .count();
        match &component[separator..index] {
            "." | "_" | "__" => {}
            dashes if dashes.bytes().all(|byte| byte == b'-') => {}
            _ => return false,
        }
    }
}

/// Whether a registry mirror is a local registry, which only Lampyr's
/// own templates could live in, rather than a cache of another registry.
fn local_registry(registry: &str, mirror: &Value) -> bool {
    let host = registry_host(registry);
    loopback(host) || (mirror["plainHttp"] == true && !public_registry(host))
}

/// The host of `registry`, a registry name with an optional port, without
/// the port or an IPv6 address's brackets.
pub(crate) fn registry_host(registry: &str) -> &str {
    match registry.strip_prefix('[') {
        Some(rest) => rest.split_once(']').map_or(rest, |(host, _)| host),
        // A bare IPv6 address has colons but no port.
        None => registry
            .rsplit_once(':')
            .filter(|(host, _)| !host.contains(':'))
            .map_or(registry, |(host, _)| host),
    }
}

/// Whether `host` is a loopback name, which Docker and zygotectl reach
/// over plain HTTP.
pub(crate) fn loopback(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

/// Whether `host` is a public registry, which a mirror keyed by it can
/// only cache, however the cluster reaches the cache.
fn public_registry(host: &str) -> bool {
    matches!(
        host,
        "docker.io"
            | "index.docker.io"
            | "registry-1.docker.io"
            | "ghcr.io"
            | "quay.io"
            | "gcr.io"
            | "registry.k8s.io"
            | "k8s.gcr.io"
            | "mcr.microsoft.com"
            | "public.ecr.aws"
            | "nvcr.io"
            | "registry.gitlab.com"
    ) || [".gcr.io", ".pkg.dev", ".azurecr.io", ".amazonaws.com"]
        .iter()
        .any(|suffix| host.ends_with(suffix))
}

/// `repository`, as the host pushes to it: a repository in a mirror's
/// in-cluster target goes through the mirror's key, since that name may
/// resolve only inside the cluster's network. Only a local mirror counts:
/// a pull-through cache's key names the registry it caches.
fn through_mirror(repository: &str, mirrors: &BTreeMap<String, Value>) -> Result<String> {
    let bare = repository.strip_prefix("http://").unwrap_or(repository);
    let Some((registry, path)) = bare.split_once('/') else {
        return Ok(repository.to_owned());
    };
    // A mirror of a registry to itself is reachable by the same name.
    let keys: Vec<&str> = mirrors
        .iter()
        .filter(|(key, mirror)| {
            key.as_str() != registry
                && mirror["registry"].as_str() == Some(registry)
                && local_registry(key, mirror)
        })
        .map(|(key, _)| key.as_str())
        .collect();
    match keys.as_slice() {
        [] => Ok(repository.to_owned()),
        [key] => Ok(pushed_through(key, mirrors, path)),
        keys => bail!(
            "several registry mirrors ({}) lead to {registry}, so Lampyr cannot pick the one \
             ly deploy pushes fiberd templates through",
            keys.join(", ")
        ),
    }
}

/// `path` in the local mirror keyed `key`, as the host pushes to it. A
/// plain HTTP mirror the host reaches by a name other than loopback keeps
/// http://, since push_reference adds it only for loopback registries.
fn pushed_through(key: &str, mirrors: &BTreeMap<String, Value>, path: &str) -> String {
    let plain_http = mirrors
        .get(key)
        .is_some_and(|mirror| mirror["plainHttp"] == true)
        && !loopback(registry_host(key));
    let scheme = if plain_http { "http://" } else { "" };
    format!("{scheme}{key}/{path}")
}

/// Why an installation whose provider makes no registry, such as AKS or
/// an existing cluster, needs zygoteRegistries.
pub(crate) fn missing_template_registry() -> &'static str {
    "the installation has no registry Lampyr can push fiberd templates to: give \
     zygoteRegistries in --provider-config, the repositories grant Pods pull templates \
     from. ly deploy pushes to the first, which must allow anonymous pulls: grant Pods \
     pull templates without credentials, and each is sealed to the cluster's fleet key"
}

/// `repository` spelled as grant Pods reach it, as the serving controller
/// rewrites a template reference through FIBERD_REGISTRY_MIRRORS.
fn mirrored(repository: &str, mirrors: &BTreeMap<String, Value>) -> Result<String> {
    let (registry, path) = repository
        .split_once('/')
        .context("a template repository needs a registry and a path")?;
    let Some(mirror) = mirrors.get(registry) else {
        return Ok(repository.to_owned());
    };
    let target = mirror["registry"]
        .as_str()
        .filter(|target| !target.trim().is_empty())
        .with_context(|| format!("registry mirror for {registry} is empty"))?;
    let scheme = if mirror["plainHttp"] == true {
        "http://"
    } else {
        ""
    };
    Ok(format!("{scheme}{target}/{path}"))
}

pub(crate) fn validate_domain(domain: &str) -> Result<()> {
    ensure!(
        !domain.is_empty()
            && domain.len() <= 253
            && domain
                .split('.')
                .all(|label| crate::project::validate_name(label).is_ok()),
        "invalid service domain"
    );
    Ok(())
}

pub(crate) fn validate_install_inputs(value: &Value) -> Result<()> {
    let domain = value["domain"].as_str().context("domain must be text")?;
    validate_domain(domain)?;
    if let Some(https_port) = value.get("httpsPort") {
        ensure!(
            https_port
                .as_u64()
                .is_some_and(|port| port <= u16::MAX as u64),
            "httpsPort must be an integer from 0 to 65535"
        );
    }
    for field in ["fiberdImage", "routerImage"] {
        let image = value[field]
            .as_str()
            .with_context(|| format!("{field} must be text"))?;
        validate_image(field, image)?;
    }
    // The platform install checks these too, but the provider has created
    // the cluster by then, so a first install with a missing or invalid one
    // would leave it behind.
    let [manifest, grant_image, helper_image] = ["controllerManifest", "grantImage", "helperImage"]
        .map(|field| {
            value[field]
                .as_str()
                .filter(|text| !text.is_empty())
                .with_context(|| format!("{field} is required: give it in --provider-config"))
        });
    validate_controller_inputs(manifest?, grant_image?, helper_image?)?;
    // The platform install defaults zygoteRegistries once the provider has
    // made the installation's registry.
    if let Some(registries) = value.get("zygoteRegistries") {
        let registries: Vec<String> = registries
            .as_array()
            .filter(|registries| !registries.is_empty())
            .and_then(|registries| {
                registries
                    .iter()
                    .map(|registry| registry.as_str().map(str::to_owned))
                    .collect()
            })
            .context(
                "zygoteRegistries must list the repositories grant Pods pull templates from",
            )?;
        // The mirrors the provider names later can change only where the
        // host pushes, not whether the GrantPolicy takes an entry, so every
        // entry is checked before the provider creates anything.
        zygote_registries(&registries)?;
    }
    Ok(())
}

fn validate_image(field: &str, image: &str) -> Result<()> {
    ensure!(
        !image.is_empty() && !image.starts_with('-') && !image.chars().any(char::is_whitespace),
        "{field} must be a valid prebuilt image reference"
    );
    Ok(())
}

/// Checks fiberd-k8s-controller's installation inputs as the platform
/// install would use them: the GrantPolicy must list each image's
/// repository, and the manifest must be one the install can read.
fn validate_controller_inputs(manifest: &str, grant_image: &str, helper_image: &str) -> Result<()> {
    crate::bootstrap::image_repository("grantImage", grant_image)?;
    crate::bootstrap::image_repository("helperImage", helper_image)?;
    validate_controller_manifest(manifest)
}

/// A controllerManifest is a local file or an https:// URL, which the
/// install downloads and applies as it is then: no digest pins it.
fn validate_controller_manifest(manifest: &str) -> Result<()> {
    if let Some(rest) = manifest.strip_prefix("https://") {
        ensure!(
            !rest.is_empty() && !rest.contains("://"),
            "controllerManifest {manifest:?} is not a valid https:// URL"
        );
        return Ok(());
    }
    // Only https:// is downloaded; read_config leaves any other URL alone,
    // and the install would otherwise look for a file of that name.
    ensure!(
        !manifest.contains("://"),
        "controllerManifest {manifest:?} must be a local file or an https:// URL: give a \
         file:// manifest as its path, and serve a downloaded one over https://"
    );
    ensure!(
        std::path::Path::new(manifest).is_file(),
        "controllerManifest {manifest} is not a file"
    );
    Ok(())
}

pub(crate) fn validate_settings(value: &Value) -> Result<()> {
    serde_json::from_value::<PlatformSettings>(value.clone())
        .context("decode platform settings")?
        .validate()
}

pub(crate) fn install(
    kube: &Kube<'_>,
    manifest_root: &std::path::Path,
    name: &str,
    provider: &str,
    installation_id: &str,
    config: Value,
) -> Result<()> {
    let settings: PlatformSettings =
        serde_json::from_value(config).context("decode platform settings")?;
    settings.validate()?;
    let templates = settings.templates()?;
    crate::bootstrap::install(
        kube,
        &crate::bootstrap::Options {
            root: manifest_root,
            name,
            provider,
            installation_id,
            domain: &settings.domain,
            https_port: settings.https_port,
            fiberd_image: &settings.fiberd_image,
            router_image: &settings.router_image,
            controller_manifest: &settings.controller_manifest,
            grant_image: &settings.grant_image,
            helper_image: &settings.helper_image,
            template_repository: &templates.repository,
            zygote_registries: &templates.registries,
            registry_mirrors: templates.mirrors,
            ingress_ports: settings.ingress_ports,
        },
    )?;
    let mut metadata = settings.metadata;
    if let Some(pod) = settings.home_pod_metadata {
        metadata.insert("homePodMetadata".into(), serde_json::to_string(&pod)?);
    }
    if !metadata.is_empty() {
        kube.patch(
            "kube-system",
            "configmap",
            "lampyr-install",
            &json!({"data":metadata}),
        )?;
    }
    Ok(())
}

/// The grant Pod label and annotation key prefixes fiberd-k8s-controller
/// refuses, from its podtemplate.reservedKeyPrefixes; it refuses keys under
/// a fiberd.io subdomain too. A grant that carries one is never admitted.
const CONTROLLER_RESERVED_KEY_PREFIXES: &[&str] = &[
    "fiberd.io/",
    "container.apparmor.security.beta.kubernetes.io/",
    "seccomp.security.alpha.kubernetes.io/",
    "container.seccomp.security.alpha.kubernetes.io/",
    "k8s.v1.cni.cncf.io/",
    "cni.projectcalico.org/",
    "io.kubernetes.cri-o.",
    "dev.gvisor.",
    "io.katacontainers.",
];

pub fn validate_metadata(value: &Value) -> Result<()> {
    let mapping = value
        .as_object()
        .context("home Pod metadata must be a mapping")?;
    for (field, entries) in mapping {
        ensure!(
            matches!(field.as_str(), "labels" | "annotations"),
            "only Pod labels and annotations may be customized"
        );
        for (name, value) in entries
            .as_object()
            .context("Pod metadata entries must be a mapping")?
        {
            ensure!(
                !name.is_empty() && name.len() <= 317 && !name.chars().any(char::is_whitespace),
                "invalid Pod metadata key"
            );
            ensure!(
                value
                    .as_str()
                    .is_some_and(|value| value.len() <= 4096 && !value.contains('\0')),
                "invalid Pod metadata value"
            );
            ensure!(
                !matches!(
                    name.as_str(),
                    "app"
                        | "lampyr.dev/home"
                        | "edge.lampyr.dev/service"
                        | "serving.knative.dev/revision"
                        | "serving.knative.dev/service"
                ),
                "provider cannot override core Pod ownership labels"
            );
            ensure!(
                !name.contains(".fiberd.io/")
                    && !CONTROLLER_RESERVED_KEY_PREFIXES
                        .iter()
                        .any(|prefix| name.starts_with(prefix)),
                "home Pod metadata key {name:?} is reserved by fiberd-k8s-controller, which would refuse every grant"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A controllerManifest that exists, as validate_install_inputs needs.
    pub(crate) fn manifest() -> &'static str {
        static MANIFEST: std::sync::LazyLock<tempfile::NamedTempFile> =
            std::sync::LazyLock::new(|| tempfile::NamedTempFile::new().unwrap());
        MANIFEST.path().to_str().unwrap()
    }

    pub(crate) fn grant_image() -> String {
        format!("registry.example/fiberd@sha256:{}", "0".repeat(64))
    }

    pub(crate) fn helper_image() -> String {
        format!("registry.example/helper@sha256:{}", "1".repeat(64))
    }

    #[test]
    fn install_inputs_reject_invalid_domains_before_cluster_mutation() {
        let settings = |domain: &str| {
            json!({
                "domain":domain,
                "httpsPort":443,
                "fiberdImage":"registry/fiberd:v1",
                "routerImage":"registry/router:v1",
                "controllerManifest":manifest(),
                "grantImage":grant_image(),
                "helperImage":helper_image(),
                "zygoteRegistries":["registry.example/templates"],
            })
        };
        assert!(validate_install_inputs(&settings("apps.example.com")).is_ok());
        let mut dynamic = settings("apps.example.com");
        dynamic["httpsPort"] = 0.into();
        assert!(validate_install_inputs(&dynamic).is_ok());
        dynamic.as_object_mut().unwrap().remove("httpsPort");
        assert!(validate_install_inputs(&dynamic).is_ok());
        for domain in [
            "UPPER.example.com",
            "bad_name.example.com",
            "-bad.example.com",
        ] {
            assert!(
                validate_install_inputs(&settings(domain)).is_err(),
                "{domain}"
            );
        }
    }

    #[test]
    fn install_inputs_require_every_controller_input() {
        let settings = json!({
            "domain":"apps.example.com",
            "fiberdImage":"registry/fiberd:v1",
            "routerImage":"registry/router:v1",
            "controllerManifest":manifest(),
            "grantImage":grant_image(),
            "helperImage":helper_image(),
            "zygoteRegistries":["registry.example/templates"],
        });
        validate_install_inputs(&settings).unwrap();
        for (field, value) in [
            ("controllerManifest", Value::Null),
            ("controllerManifest", json!("")),
            // The platform install would fail to read these only once the
            // provider has made the cluster.
            ("controllerManifest", json!("/nonexistent/controller.yaml")),
            ("controllerManifest", json!(std::env::temp_dir())),
            ("controllerManifest", json!("file:///controller.yaml")),
            (
                "controllerManifest",
                json!("http://example.com/controller.yaml"),
            ),
            ("controllerManifest", json!("https://")),
            (
                "controllerManifest",
                json!("https://http://example.com/c.yaml"),
            ),
            ("grantImage", Value::Null),
            ("grantImage", json!([grant_image()])),
            ("helperImage", json!("")),
            // The GrantPolicy lists only digest-pinned images from fully
            // qualified repositories.
            ("helperImage", json!("ghcr.io/team/helper:v1")),
            ("helperImage", json!("ghcr.io/team/helper@sha256:00")),
            (
                "grantImage",
                json!(format!("registry/fiberd@sha256:{}", "0".repeat(64))),
            ),
            (
                "helperImage",
                json!(format!("ghcr.io/Team/helper@sha256:{}", "1".repeat(64))),
            ),
            ("zygoteRegistries", json!([])),
            ("zygoteRegistries", json!("registry.example/templates")),
            (
                "zygoteRegistries",
                json!(["registry.example/templates", ""]),
            ),
            // The GrantPolicy would refuse these only once the provider
            // has made the cluster: a bare registry, a trailing slash, a
            // host the CRD takes for a path, an uppercase path, a tag.
            ("zygoteRegistries", json!(["myacr.azurecr.io"])),
            ("zygoteRegistries", json!(["ghcr.io/team/"])),
            ("zygoteRegistries", json!(["myteam/templates"])),
            ("zygoteRegistries", json!(["ghcr.io/Team/templates"])),
            ("zygoteRegistries", json!(["ghcr.io/team/templates:v1"])),
            // Every entry is checked, not only the one templates go to.
            (
                "zygoteRegistries",
                json!(["registry.example/templates", "ghcr.io/team/"]),
            ),
            (
                "zygoteRegistries",
                json!(["registry.example/templates", "my_registry:5000/templates"]),
            ),
            (
                "zygoteRegistries",
                json!(["registry.example/t", "https://registry.example/t"]),
            ),
            (
                "zygoteRegistries",
                json!(vec!["registry.example/templates"; 65]),
            ),
            (
                "zygoteRegistries",
                json!([format!("registry.example/{}", "a".repeat(240))]),
            ),
            // Stripping https:// must not leave a plain HTTP entry.
            (
                "zygoteRegistries",
                json!(["https://http://registry.example/templates"]),
            ),
        ] {
            let mut missing = settings.clone();
            if value.is_null() {
                missing.as_object_mut().unwrap().remove(field);
            } else {
                missing[field] = value.clone();
            }
            let error = validate_install_inputs(&missing).unwrap_err().to_string();
            assert!(error.contains(field), "{field} = {value}: {error}");
        }
        // The platform install defaults it once the provider has run.
        let mut defaulted = settings.clone();
        defaulted
            .as_object_mut()
            .unwrap()
            .remove("zygoteRegistries");
        validate_install_inputs(&defaulted).unwrap();
        // A downloaded manifest is applied as the URL serves it.
        let mut downloaded = settings.clone();
        downloaded["controllerManifest"] = json!("https://example.com/controller.yaml");
        validate_install_inputs(&downloaded).unwrap();
    }

    #[test]
    fn zygote_registries_take_the_grant_policy_s_registry_pattern() {
        // fiberd-k8s-controller's TestRegistryPattern cases, and more.
        for (registry, want) in [
            ("ghcr.io/team-a/zygotes", true),
            ("http://registry.local:5000/zygotes", true),
            ("localhost:5000/zygotes", true),
            ("localhost/zygotes", true),
            ("registry:5000/ci", true),
            ("ghcr.io/team__a/z.y-g--o_tes", true),
            ("https://ghcr.io/team-a/zygotes", false),
            ("HTTP://ghcr.io/team-a/zygotes", false),
            ("http://ghcr.io", false),
            ("ghcr.io/team-a/zygotes:latest", false),
            ("ghcr.io/team-a/zygotes@sha256:0123", false),
            ("ghcr.io", false),
            ("zygotes", false),
            ("oci://ghcr.io/team-a/zygotes", false),
            ("ghcr.io/team-a/zygotes/", false),
            ("", false),
            ("registry/templates", false),
            ("Ghcr.io/team", false),
            ("-ghcr.io/team", false),
            ("ghcr.io:123456/team", false),
            ("ghcr.io:/team", false),
            ("ghcr.io/team--/a", false),
            ("ghcr.io/te___am", false),
            ("ghcr.io/te._am", false),
            ("ghcr.io/tëam", false),
        ] {
            assert_eq!(registry_pattern(registry), want, "{registry}");
        }
        // TLS is the default, so https:// is the bare spelling.
        assert_eq!(
            zygote_registries(&["https://ghcr.io/team".to_owned()]).unwrap(),
            ["ghcr.io/team"]
        );
    }

    #[test]
    fn an_existing_cluster_install_checks_every_zygote_registry() {
        // ly cluster install --context checks the settings before it claims
        // the cluster, and templates() alone looks at only one entry.
        let settings = |registries: Value| {
            json!({
                "domain":"apps.example.com",
                "httpsPort":443,
                "fiberdImage":"registry/fiberd:v1",
                "routerImage":"registry/router:v1",
                "controllerManifest":manifest(),
                "grantImage":grant_image(),
                "helperImage":helper_image(),
                "zygoteRegistries":registries,
            })
        };
        validate_settings(&settings(json!(["registry.example/templates"]))).unwrap();
        for registries in [
            json!(["registry.example/templates", "ghcr.io/team/"]),
            json!(["registry.example/templates", "ghcr.io"]),
            json!(["registry.example/templates", "myteam/templates"]),
        ] {
            let error = validate_settings(&settings(registries.clone()))
                .unwrap_err()
                .to_string();
            assert!(error.contains("zygoteRegistries"), "{registries}: {error}");
        }
    }

    #[test]
    fn templates_go_to_the_one_registry_the_provider_made() {
        let no_mirrors = BTreeMap::new();
        // Kind and k3d mirror their local registry to the name the cluster
        // reaches it at.
        let mirrors = BTreeMap::from([(
            "localhost:5002".to_owned(),
            json!({"registry": "lampyr-platform-1:5000", "plainHttp": true}),
        )]);
        let kind = Templates {
            repository: "localhost:5002/lampyr/fiberd-templates".into(),
            registries: vec!["http://lampyr-platform-1:5000/lampyr/fiberd-templates".into()],
            mirrors: mirrors.clone(),
        };
        assert_eq!(templates(&[], &mirrors).unwrap(), kind);
        // A given list that admits where grant Pods pull from the mirror
        // keeps the mirror's repository, which the host can push to.
        let given = ["http://lampyr-platform-1:5000/lampyr".to_owned()];
        assert_eq!(
            templates(&given, &mirrors).unwrap(),
            Templates {
                registries: given.to_vec(),
                ..kind
            }
        );
        // Any other given list names its own repository. One in the
        // mirror's target, which the host may not resolve, is pushed
        // through the mirror's key.
        let given = ["http://lampyr-platform-1:5000/lamp".to_owned()];
        assert_eq!(
            templates(&given, &mirrors).unwrap(),
            Templates {
                repository: "localhost:5002/lamp".into(),
                registries: given.to_vec(),
                mirrors: mirrors.clone(),
            }
        );
        // Either way, the list must admit where grant Pods pull it: over
        // plain HTTP from the target, not TLS or the host's name.
        for given in [
            "https://lampyr-platform-1:5000/lampyr",
            "localhost:5002/lampyr",
        ] {
            let error = templates(&[given.to_owned()], &mirrors)
                .unwrap_err()
                .to_string();
            assert!(error.contains("must admit"), "{given}: {error}");
        }
        // A lone mirror unrelated to templates, such as a pull-through
        // cache, does not take the given repository's place.
        let cache = BTreeMap::from([(
            "docker.io".to_owned(),
            json!({"registry": "cache.internal:5000"}),
        )]);
        let given = ["ghcr.io/team/templates".to_owned()];
        assert_eq!(
            templates(&given, &cache).unwrap(),
            Templates {
                repository: "ghcr.io/team/templates".into(),
                registries: given.to_vec(),
                mirrors: cache.clone(),
            }
        );
        // Nor does a list that admits the cache's target: pushing through
        // its key would push to Docker Hub.
        let given = ["cache.internal:5000/lampyr".to_owned()];
        assert_eq!(
            templates(&given, &cache).unwrap(),
            Templates {
                repository: "cache.internal:5000/lampyr".into(),
                registries: given.to_vec(),
                mirrors: cache.clone(),
            }
        );
        // Without a given list, such a mirror is no template registry: ly
        // deploy would push to the registry it caches. Reaching the cache
        // over plain HTTP doesn't make it local.
        let error = templates(&[], &cache).unwrap_err().to_string();
        assert!(error.contains("zygoteRegistries"), "{error}");
        let plain_cache = BTreeMap::from([(
            "docker.io".to_owned(),
            json!({"registry": "cache.internal:5000", "plainHttp": true}),
        )]);
        let error = templates(&[], &plain_cache).unwrap_err().to_string();
        assert!(error.contains("zygoteRegistries"), "{error}");
        let given = ["http://cache.internal:5000/lampyr".to_owned()];
        assert_eq!(
            templates(&given, &plain_cache).unwrap().repository,
            "http://cache.internal:5000/lampyr"
        );
        let mut cached = mirrors.clone();
        cached.extend(cache.clone());
        assert_eq!(
            templates(&[], &cached).unwrap().repository,
            "localhost:5002/lampyr/fiberd-templates"
        );
        // A local registry is one the host reaches on loopback, or one the
        // cluster reaches over plain HTTP.
        for registry in ["127.0.0.1:5000", "[::1]:5000", "localhost"] {
            let local = BTreeMap::from([(
                registry.to_owned(),
                json!({"registry": "registry.internal:5000"}),
            )]);
            assert_eq!(
                templates(&[], &local).unwrap().repository,
                format!("{registry}/lampyr/fiberd-templates")
            );
        }
        assert_eq!(
            templates(
                &["http://lampyr-platform-1:5000/lampyr".to_owned()],
                &cached
            )
            .unwrap()
            .repository,
            "localhost:5002/lampyr/fiberd-templates"
        );
        // Without a registry Lampyr knows, such as on AKS, the first given
        // repository is the one ly deploy pushes to.
        let error = templates(&[], &no_mirrors).unwrap_err().to_string();
        assert!(
            error.contains("zygoteRegistries") && error.contains("anonymous"),
            "{error}"
        );
        let given = [
            "ghcr.io/team/templates".to_owned(),
            "ghcr.io/other".to_owned(),
        ];
        assert_eq!(
            templates(&given, &no_mirrors).unwrap(),
            Templates {
                repository: "ghcr.io/team/templates".into(),
                registries: given.to_vec(),
                mirrors: BTreeMap::new(),
            }
        );
        // TLS is the default, which the GrantPolicy does not spell.
        assert_eq!(
            templates(&["https://reg:5000/templates".to_owned()], &no_mirrors).unwrap(),
            Templates {
                repository: "reg:5000/templates".into(),
                registries: vec!["reg:5000/templates".into()],
                mirrors: BTreeMap::new(),
            }
        );
        // A template reference has no scheme, so grant Pods reach a plain
        // HTTP repository through a mirror of its registry to itself.
        assert_eq!(
            templates(&["http://reg:5000/templates".to_owned()], &no_mirrors).unwrap(),
            Templates {
                repository: "http://reg:5000/templates".into(),
                registries: vec!["http://reg:5000/templates".into()],
                mirrors: BTreeMap::from([(
                    "reg:5000".to_owned(),
                    json!({"registry": "reg:5000", "plainHttp": true}),
                )]),
            }
        );
        let mut both = mirrors.clone();
        both.insert("localhost:5003".into(), json!({"registry": "other:5000"}));
        assert!(templates(&[], &both).is_err());
        // The default zygoteRegistries entry must suit the GrantPolicy too.
        let unlisted = BTreeMap::from([(
            "localhost:5002".to_owned(),
            json!({"registry": "registry_internal:5000", "plainHttp": true}),
        )]);
        let error = format!("{:#}", templates(&[], &unlisted).unwrap_err());
        assert!(error.contains("zygoteRegistries"), "{error}");
        // A plain HTTP mirror keyed by a name other than loopback is pushed
        // to over plain HTTP too, however Lampyr picks it.
        let lan = BTreeMap::from([(
            "registry.lan:5000".to_owned(),
            json!({"registry": "registry.internal:5000", "plainHttp": true}),
        )]);
        let pushed = "http://registry.lan:5000/lampyr/fiberd-templates";
        assert_eq!(
            templates(&[], &lan).unwrap(),
            Templates {
                repository: pushed.into(),
                registries: vec!["http://registry.internal:5000/lampyr/fiberd-templates".into()],
                mirrors: lan.clone(),
            }
        );
        let given = ["http://registry.internal:5000/lampyr".to_owned()];
        assert_eq!(templates(&given, &lan).unwrap().repository, pushed);
        let given = ["http://registry.internal:5000/other".to_owned()];
        assert_eq!(
            templates(&given, &lan).unwrap().repository,
            "http://registry.lan:5000/other"
        );
        assert_eq!(
            crate::fiberd_template::push_reference(pushed, "demo-0123456789ab").unwrap(),
            "http://registry.lan:5000/lampyr/fiberd-templates:demo-0123456789ab"
        );
    }

    #[test]
    fn provider_metadata_cannot_replace_core_manifest_inventory() {
        let settings = json!({
            "domain":"apps.example.com",
            "httpsPort":443,
            "fiberdImage":"registry/fiberd:v1",
            "routerImage":"registry/router:v1",
            "controllerManifest":manifest(),
            "grantImage":grant_image(),
            "helperImage":helper_image(),
            "metadata":{
                crate::bootstrap::CORE_INVENTORY_KEY:"attacker-controlled"
            },
        });
        assert!(
            validate_settings(&settings)
                .unwrap_err()
                .to_string()
                .contains(crate::bootstrap::CORE_INVENTORY_KEY)
        );
    }

    #[test]
    fn provider_home_metadata_preserves_knative_ownership_labels() {
        assert!(
            validate_metadata(&json!({
                "labels":{"azure.workload.identity/use":"true"},
                "annotations":{"example.dev/profile":"private"}
            }))
            .is_ok()
        );
        for label in [
            "app",
            "lampyr.dev/home",
            "serving.knative.dev/revision",
            "serving.knative.dev/service",
        ] {
            assert!(
                validate_metadata(&json!({"labels":{(label):"override"}})).is_err(),
                "{label}"
            );
        }
        for key in [
            "container.apparmor.security.beta.kubernetes.io/fiberd",
            "k8s.v1.cni.cncf.io/networks",
            "fiberd.io/grant",
            "policy.fiberd.io/tier",
        ] {
            let error = validate_metadata(&json!({"annotations":{(key):"value"}}))
                .expect_err(key)
                .to_string();
            assert!(
                error.contains("reserved by fiberd-k8s-controller"),
                "{error}"
            );
        }
    }
}
