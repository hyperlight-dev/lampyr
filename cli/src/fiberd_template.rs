// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

//! The fiberd template artifact: the same Hyperlight snapshot `ly build`
//! makes, sealed by fiberd's `zygotectl` into the artifact stock fiberd's
//! Hyperlight backend pulls by digest and hands to `lampyr-hluk-helper` as
//! its `--template-dir`.
//!
//! The build records the helper's `--version` facts where the snapshot was
//! made, since only a KVM host of the same CPU vendor reports the facts a
//! grant Pod's helper will. They are part of the Revision's identity, so a
//! rebuild for another helper, or with a template where there was none, is
//! a new Revision rather than the same immutable one. Deploy seals the
//! template to the cluster's fleet key and pushes it, because only then are
//! the cluster and registry known.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};

use crate::host::Host;
use crate::image::{self, Metadata};
use crate::kube::{Kube, SYSTEM};

/// The Revision annotation naming the sealed template, `<repository>@<digest>`,
/// from which a CapacityGrant's `spec.zygote.{registry,digest}` are filled.
pub const ANNOTATION: &str = "lampyr.dev/fiberd-template";
/// The artifact metadata's runtime entry holding the facts line of the
/// helper's `--version`.
pub const RUNTIME_FACTS: &str = "fiberdHelper";
/// The Secret grant Pods mount as fiberd's `-delta-seal-key`. It lives
/// where Lampyr's grant Pods run, since a Pod mounts only its own
/// namespace's Secrets.
pub const SEAL_KEY_SECRET: &str = "lampyr-fiberd-seal-key";
pub const SEAL_KEY_NAMESPACE: &str = "default";
pub const SEAL_KEY_DATA: &str = "seal-key.json";
/// The public half authors seal to. It opens nothing, so any builder may
/// read it.
pub const FLEET_PUB_CONFIGMAP: &str = "lampyr-fiberd-fleet-pub";
pub const FLEET_PUB_DATA: &str = "fleet-pub.json";

const HELPER: &str = "build/hluk-helper";
const ZYGOTECTL: &str = "build/zygotectl";
const GRANT_ISSUER: &str = "build/grant-issuer";
const HELPER_NAME: &str = "lampyr-hluk-helper/";
/// The capability line a helper prints after its facts when it restores
/// the template fiberd unpacks. fiberd passes `--template-dir` only to a
/// helper that prints it, so a template for any other helper would never
/// warm.
const TEMPLATE_DIR_CAPABILITY: &str = "template-dir";

/// The helper of the platform at `root`, or `None` when the platform has
/// none, which leaves `ly build` and `ly dev` usable without the template
/// tools. Anything at the path that is not a file is an error rather than
/// "absent", since skipping it would hide a broken platform.
pub fn helper(root: &Path) -> Result<Option<PathBuf>> {
    let path = root.join(HELPER);
    match std::fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("inspect {}", path.display())),
        Ok(_) => {
            // Follows a symlink, so a dangling one is refused too.
            ensure!(path.is_file(), "{} is not a file", path.display());
            Ok(Some(path))
        }
    }
}

/// What `ly build` prints when the platform has no helper: the artifact
/// is fine for local use, so this is a note rather than an error.
pub fn missing_helper_note(root: &Path) -> String {
    format!(
        "note: {} is missing, so this artifact cannot be deployed to a cluster; rebuild where the platform has it (in a checkout, run `just template-tools`)",
        root.join(HELPER).display()
    )
}

/// A fiberd template tool of the platform at `root`. A platform package
/// carries them, built for its target; a checkout builds them with
/// `just fiberd-template-tools`.
fn tool(root: &Path, relative: &str) -> Result<PathBuf> {
    let path = root.join(relative);
    ensure!(
        path.is_file(),
        "{} is missing; install a platform that carries it with ly setup, or in a checkout run `just template-tools`",
        path.display()
    );
    Ok(path)
}

/// Checks a helper's whole `--version` output: the facts, which must match
/// the snapshot, then `template-dir`, without which fiberd would not pass
/// the helper its template. Stricter than fiberd, which accepts
/// `template-dir` on any trimmed line after the facts: this requires it on
/// the second line, the form fiberd's PROTOCOL.md describes (as of fiberd
/// commit 549f616) and the helper prints.
/// Returns the facts line alone, which is what fiberd and zygotectl read.
pub fn version_facts(output: &str, metadata: &Metadata) -> Result<String> {
    let mut lines = output.lines();
    let first = lines.next().unwrap_or_default();
    ensure!(
        lines.next() == Some(TEMPLATE_DIR_CAPABILITY),
        "helper {:?} does not print the {TEMPLATE_DIR_CAPABILITY:?} capability, so fiberd would not pass it the template; rebuild it with `just template-tools`",
        first.split(' ').next().unwrap_or_default()
    );
    facts(first, metadata)
}

/// Checks a helper's facts line, the first of its `--version`, against the
/// snapshot it will restore, so a template no grant Pod's helper could
/// warm is refused at build time rather than at warm.
pub fn facts(line: &str, metadata: &Metadata) -> Result<String> {
    let line = line.lines().next().unwrap_or_default().trim();
    let tokens: Vec<&str> = line.split(' ').collect();
    ensure!(
        tokens.len() == 4 && tokens.iter().all(|token| !token.is_empty()),
        "helper facts {line:?} are not four tokens"
    );
    let commit = metadata
        .runtime
        .get("hyperlightUnikraft")
        .context("artifact metadata has no hyperlight-unikraft commit")?;
    // A short or empty commit would match helpers of other commits.
    ensure!(
        commit.len() == 40
            && commit
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "artifact metadata names hyperlight-unikraft commit {commit:?}, not a full commit"
    );
    ensure!(
        tokens[0].starts_with(HELPER_NAME)
            && tokens[0].ends_with(&format!("+hluk-{}", &commit[..12])),
        "helper {:?} does not restore snapshots made by hyperlight-unikraft {commit}",
        tokens[0]
    );
    // A helper run without /dev/kvm reports "none", which no grant Pod has.
    ensure!(
        tokens[2] == "kvm",
        "helper reported hypervisor {:?}; run it where /dev/kvm is available",
        tokens[2]
    );
    let vendor = metadata
        .runtime
        .get("cpuVendor")
        .context("artifact metadata has no CPU vendor")?;
    ensure!(
        tokens[3] == vendor,
        "helper ran on {} but the snapshot was made on {vendor}",
        tokens[3]
    );
    Ok(line.to_owned())
}

/// Checks a helper's `--version` output against the snapshot and records
/// its facts line in the artifact metadata, which recomputes the Revision
/// ID: helper facts the ID did not cover would let a rebuild for a new
/// helper reuse a Revision whose template it can never change.
pub fn record_facts(metadata: &mut Metadata, output: &str) -> Result<()> {
    let facts = version_facts(output, metadata)?;
    metadata.record_runtime(RUNTIME_FACTS, facts)
}

/// The facts the build recorded in the artifact metadata, if it was built
/// with the platform's helper. They are rechecked whole, since the line goes
/// to zygotectl as it is.
pub fn recorded_facts(metadata: &Metadata) -> Result<Option<String>> {
    let Some(line) = metadata.runtime.get(RUNTIME_FACTS) else {
        return Ok(None);
    };
    let checked = facts(line, metadata)?;
    ensure!(
        checked == *line,
        "recorded helper facts {line:?} are not a single facts line"
    );
    Ok(Some(checked))
}

/// Lays out the tree the helper's `Template::load` reads, `metadata.json`
/// and `snapshot/`, at `tree`, which must not exist. Files are hard links
/// where the filesystem allows, since zygotectl only reads them and
/// snapshots are as large as guest memory.
pub fn assemble(artifact: &Path, tree: &Path) -> Result<()> {
    std::fs::create_dir(tree).with_context(|| format!("create {}", tree.display()))?;
    link_or_copy(&artifact.join("metadata.json"), &tree.join("metadata.json"))?;
    copy_tree(&artifact.join("snapshot"), &tree.join("snapshot"))
}

fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    let kind = std::fs::symlink_metadata(source)
        .with_context(|| format!("inspect {}", source.display()))?
        .file_type();
    ensure!(
        kind.is_dir(),
        "{} must be a real directory",
        source.display()
    );
    std::fs::create_dir(destination)?;
    let mut entries = std::fs::read_dir(source)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let kind = entry.file_type()?;
        let to = destination.join(entry.file_name());
        if kind.is_dir() {
            copy_tree(&entry.path(), &to)?;
        } else if kind.is_file() {
            link_or_copy(&entry.path(), &to)?;
        } else {
            // zygotectl tars directories and regular files only.
            bail!(
                "snapshot entry {} is neither a directory nor a regular file",
                entry.path().display()
            );
        }
    }
    Ok(())
}

fn link_or_copy(source: &Path, destination: &Path) -> Result<()> {
    ensure!(
        std::fs::symlink_metadata(source)
            .with_context(|| format!("inspect {}", source.display()))?
            .is_file(),
        "{} must be a regular file",
        source.display()
    );
    if std::fs::hard_link(source, destination).is_err() {
        std::fs::copy(source, destination).with_context(|| format!("copy {}", source.display()))?;
    }
    Ok(())
}

/// A sealed template in a registry, digest-pinned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reference {
    pub repository: String,
    pub digest: String,
}

impl Reference {
    pub fn parse(value: &str) -> Result<Self> {
        let (repository, digest) = value
            .split_once('@')
            .with_context(|| format!("{ANNOTATION} {value:?} is not digest-pinned"))?;
        ensure!(
            image::is_sha256_digest(digest),
            "{ANNOTATION} {value:?} has an invalid digest"
        );
        ensure!(
            !repository.is_empty()
                && !repository.contains("://")
                && !repository.starts_with('-')
                && !repository.chars().any(char::is_whitespace),
            "{ANNOTATION} {value:?} has an invalid repository"
        );
        Ok(Self {
            repository: repository.to_owned(),
            digest: digest.to_owned(),
        })
    }
}

impl std::fmt::Display for Reference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}", self.repository, self.digest)
    }
}

/// Records the template on a Knative Service's Revision template.
pub fn annotate(service: &mut Value, reference: &Reference) -> Result<()> {
    service
        .pointer_mut("/spec/template/metadata/annotations")
        .and_then(Value::as_object_mut)
        .context("Knative Service has no Revision annotations")?
        .insert(ANNOTATION.into(), reference.to_string().into());
    Ok(())
}

/// The template a Revision or Revision template records, if any.
pub fn recorded(revision: &Value) -> Result<Option<Reference>> {
    revision
        .pointer(&format!(
            "/metadata/annotations/{}",
            ANNOTATION.replace('/', "~1")
        ))
        .map(|value| Reference::parse(value.as_str().context("template annotation is not text")?))
        .transpose()
}

/// Where the template is pushed: the installation's template repository,
/// under a tag named for the Revision, so templates of every Service share
/// one repository and each is found by its Revision. zygotectl speaks plain
/// HTTP only when told: to a repository the install was given as http://,
/// and to loopback registries, which Docker also trusts without TLS.
pub fn push_reference(repository: &str, revision: &str) -> Result<String> {
    let (plain_http, repository) = without_scheme(repository);
    let target = format!("{repository}:{revision}");
    crate::registry::validate_tag(&target)
        .with_context(|| format!("invalid fiberd template reference {target:?}"))?;
    let registry = repository.split('/').next().unwrap_or_default();
    let loopback = repository.contains('/')
        && crate::platform_host::loopback(crate::platform_host::registry_host(registry));
    let scheme = if plain_http || loopback {
        "http://"
    } else {
        ""
    };
    Ok(format!("{scheme}{target}"))
}

/// A template repository and whether it is plain HTTP, which the install
/// records as http:// since a template reference carries no scheme.
fn without_scheme(repository: &str) -> (bool, &str) {
    match repository.strip_prefix("http://") {
        Some(repository) => (true, repository),
        None => (false, repository),
    }
}

/// Seals the artifact's snapshot to the cluster's fleet key, pushes it to
/// the installation's template `repository` for `revision`, and returns
/// its digest-pinned reference.
pub fn publish(
    kube: &Kube<'_>,
    root: &Path,
    artifact: &Path,
    facts: &str,
    repository: &str,
    revision: &str,
    host: &Host,
) -> Result<Reference> {
    let zygotectl = tool(root, ZYGOTECTL)?;
    let target = push_reference(repository, revision)?;
    let public = ensure_fleet_key(kube, root)?;
    let work = tempfile::Builder::new()
        .prefix("lampyr-fiberd-")
        .tempdir()?;
    let seal_to = work.path().join(FLEET_PUB_DATA);
    std::fs::write(&seal_to, public)?;
    let tree = work.path().join("template");
    assemble(artifact, &tree)?;
    let out = work.path().join("artifact");
    let built = host.capture(
        host.command(&zygotectl)?
            .arg("build")
            .arg("-hyperlight-snapshot")
            .arg(&tree)
            .args([
                "-hyperlight-facts",
                facts,
                "-hyperlight-arch",
                "amd64",
                "-seal-to",
            ])
            .arg(&seal_to)
            .arg("-out")
            .arg(&out),
    )?;
    let pushed = host.capture(
        host.command(&zygotectl)?
            .args(["push", "-dir"])
            .arg(&out)
            .args(["-ref", &target]),
    )?;
    let digest = pushed.lines().last().unwrap_or_default().trim();
    ensure!(
        built.lines().last().map(str::trim) == Some(digest),
        "zygotectl pushed {digest:?}, not the artifact it built"
    );
    let (_, repository) = without_scheme(repository);
    Reference::parse(&format!("{repository}@{digest}"))
}

fn validate_public(encoded: &[u8]) -> Result<()> {
    let key: Value = serde_json::from_slice(encoded).context("decode fleet public key")?;
    ensure!(
        key["kty"] == "OKP"
            && key["crv"] == "X25519"
            && key["kid"].is_string()
            && key.get("d").is_none(),
        "fleet public key is not a public X25519 JWK"
    );
    Ok(())
}

fn validate_seal_key(encoded: &[u8]) -> Result<()> {
    let key: Value = serde_json::from_slice(encoded).context("decode fleet seal key")?;
    ensure!(
        key["kty"] == "oct" && key["alg"] == "A256GCM" && key["k"].is_string(),
        "fleet seal key is not an A256GCM JWK"
    );
    Ok(())
}

/// The fleet seal key's Secret and its public half's ConfigMap. The Secret
/// keeps a copy of the public half so a lost ConfigMap can be restored
/// without the private key ever leaving the cluster again.
pub fn key_resources(seal_key: &[u8], fleet_pub: &[u8]) -> Result<(Value, Value)> {
    validate_seal_key(seal_key)?;
    validate_public(fleet_pub)?;
    let labels = json!({ "app.kubernetes.io/managed-by": "lampyr" });
    let secret = json!({
        "apiVersion": "v1", "kind": "Secret", "type": "Opaque",
        "metadata": { "name": SEAL_KEY_SECRET, "namespace": SEAL_KEY_NAMESPACE, "labels": labels },
        "data": {
            SEAL_KEY_DATA: STANDARD.encode(seal_key),
            FLEET_PUB_DATA: STANDARD.encode(fleet_pub),
        }
    });
    Ok((secret, public_resource(fleet_pub)?))
}

fn public_resource(fleet_pub: &[u8]) -> Result<Value> {
    Ok(json!({
        "apiVersion": "v1", "kind": "ConfigMap",
        "metadata": {
            "name": FLEET_PUB_CONFIGMAP, "namespace": SYSTEM,
            "labels": { "app.kubernetes.io/managed-by": "lampyr" }
        },
        "data": { FLEET_PUB_DATA: std::str::from_utf8(fleet_pub)? }
    }))
}

/// What deploy finds of the fleet key, and so what it must do before it
/// seals a template.
#[derive(Debug, PartialEq, Eq)]
enum FleetKey {
    /// The Secret and the ConfigMap agree on this public key.
    Ready(Vec<u8>),
    /// The ConfigMap is gone, and the Secret's copy of this public key
    /// restores it.
    Restore(Vec<u8>),
    Create,
}

/// Decides from the Secret's copy of the public key, as `secret_public`
/// reads it, and the ConfigMap. The Secret is the truth, since grant Pods
/// open templates with it: a ConfigMap that differs, stale or tampered
/// with, would have templates sealed to a key they can't open, or that
/// another key can.
fn fleet_key(secret: Option<Option<&str>>, configmap: Option<&Value>) -> Result<FleetKey> {
    let Some(secret) = secret else {
        ensure!(
            configmap.is_none(),
            "{SYSTEM}/{FLEET_PUB_CONFIGMAP} exists without {SEAL_KEY_NAMESPACE}/{SEAL_KEY_SECRET}; templates sealed to it cannot be opened. Delete the ConfigMap to create a new fleet key"
        );
        return Ok(FleetKey::Create);
    };
    let encoded = secret.context("fleet seal key Secret has no fleet-pub.json")?;
    let public = STANDARD.decode(encoded).context("decode fleet-pub.json")?;
    validate_public(&public)?;
    let Some(configmap) = configmap else {
        return Ok(FleetKey::Restore(public));
    };
    let published = configmap
        .pointer(&format!("/data/{}", FLEET_PUB_DATA.replace('/', "~1")))
        .and_then(Value::as_str)
        .context("fleet public key ConfigMap has no fleet-pub.json")?;
    ensure!(
        published.as_bytes() == public.as_slice(),
        "{SYSTEM}/{FLEET_PUB_CONFIGMAP} differs from the public key in {SEAL_KEY_NAMESPACE}/{SEAL_KEY_SECRET}, which grant Pods open templates with; delete the ConfigMap so the next deploy restores it from the Secret"
    );
    Ok(FleetKey::Ready(public))
}

/// The seal key Secret's copy of the public key: None without the Secret,
/// Some(None) if it lacks one. Only that field is read, so the private key
/// never reaches the host.
fn secret_public(kube: &Kube<'_>) -> Result<Option<Option<String>>> {
    let output = kube.capture(kube.command(Some(SEAL_KEY_NAMESPACE))?.args([
        "get",
        "secret",
        SEAL_KEY_SECRET,
        "--ignore-not-found=true",
        "--output",
        &format!(
            "jsonpath={{.metadata.name}}{{\" \"}}{{.data.{}}}",
            FLEET_PUB_DATA.replace('.', "\\.")
        ),
    ]))?;
    Ok(parse_secret_public(&output))
}

fn parse_secret_public(output: &str) -> Option<Option<String>> {
    if output.is_empty() {
        return None;
    }
    Some(
        output
            .split_once(' ')
            .map(|(_, public)| public.trim().to_owned())
            .filter(|public| !public.is_empty()),
    )
}

/// Creates the fleet seal key once, on the first deploy that needs it, so
/// clusters that never run stock fiberd carry no key, and returns the
/// public key to seal to. A key is never replaced, since every template
/// sealed to it would stop opening.
pub fn ensure_fleet_key(kube: &Kube<'_>, root: &Path) -> Result<Vec<u8>> {
    let secret = secret_public(kube)?;
    let configmap = kube.optional(SYSTEM, "configmap", FLEET_PUB_CONFIGMAP)?;
    match fleet_key(secret.as_ref().map(Option::as_deref), configmap.as_ref())? {
        FleetKey::Ready(public) => Ok(public),
        FleetKey::Restore(public) => {
            kube.apply(&public_resource(&public)?)?;
            Ok(public)
        }
        FleetKey::Create => {
            let issuer = tool(root, GRANT_ISSUER)?;
            let work = tempfile::Builder::new()
                .prefix("lampyr-fleet-key-")
                .tempdir()?;
            let seal = work.path().join(SEAL_KEY_DATA);
            let public = work.path().join(FLEET_PUB_DATA);
            kube.host.run(
                kube.host
                    .command(&issuer)?
                    .args(["keygen", "-alg", "A256GCM", "-out"])
                    .arg(&seal)
                    .arg("-fleet-pub")
                    .arg(&public),
            )?;
            let seal_key = zeroize::Zeroizing::new(std::fs::read(&seal)?);
            let public = std::fs::read(&public)?;
            let (secret, configmap) = key_resources(&seal_key, &public)?;
            // create, not apply: apply would copy the private key into the
            // last-applied annotation, and would replace a racing key.
            kube.create(&secret)?;
            kube.apply(&configmap)?;
            Ok(public)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn helper_is_optional_but_must_be_a_file_when_present() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(helper(root.path()).unwrap(), None);
        assert!(missing_helper_note(root.path()).contains("just template-tools"));

        let path = root.path().join(HELPER);
        std::fs::create_dir_all(&path).unwrap();
        let error = helper(root.path()).unwrap_err().to_string();
        assert!(error.contains("is not a file"), "{error}");
        std::fs::remove_dir(&path).unwrap();

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.path().join("gone"), &path).unwrap();
            let error = helper(root.path()).unwrap_err().to_string();
            assert!(error.contains("is not a file"), "{error}");
            std::fs::remove_file(&path).unwrap();
        }

        std::fs::write(&path, "").unwrap();
        assert_eq!(helper(root.path()).unwrap(), Some(path));
    }

    const COMMIT: &str = "df5ba045260063ce14a63c1a713efb34ecf7a5bf";

    fn metadata() -> Metadata {
        let project = {
            let directory = tempfile::tempdir().unwrap();
            let service = directory.path().join("demo");
            crate::project::initialize(&service, Some("demo"), "python").unwrap();
            crate::project::Project::load(&service).unwrap()
        };
        let mut metadata = Metadata::create(
            project,
            format!("sha256:{}", "ab".repeat(32)),
            BTreeMap::from([
                ("kernel".to_owned(), format!("sha256:{}", "01".repeat(32))),
                ("rootfs".to_owned(), format!("sha256:{}", "02".repeat(32))),
                ("snapshot".to_owned(), format!("sha256:{}", "03".repeat(32))),
            ]),
            "GenuineIntel".to_owned(),
        )
        .unwrap();
        metadata
            .runtime
            .insert("hyperlightUnikraft".to_owned(), COMMIT.to_owned());
        metadata
    }

    #[test]
    fn only_the_facts_line_of_a_template_dir_helper_s_version_is_kept() {
        let metadata = metadata();
        let line =
            "lampyr-hluk-helper/0.3.0+hluk-df5ba0452600 hyperlight_host/0.19.0 kvm GenuineIntel";
        for output in [
            format!("{line}\ntemplate-dir\n"),
            format!("{line}\ntemplate-dir"),
            format!("{line}\r\ntemplate-dir\r\n"),
        ] {
            assert_eq!(
                version_facts(&output, &metadata).unwrap(),
                line,
                "{output:?}: zygotectl -hyperlight-facts takes the first line alone"
            );
        }
        for (output, want) in [
            (format!("{line}\n"), "capability"),
            (format!("{line}\ntemplate-dirs\n"), "capability"),
            (format!("{line}\nother\ntemplate-dir\n"), "capability"),
            (format!("{line}\n template-dir\n"), "capability"),
            ("template-dir\n".to_owned(), "capability"),
            (format!("template-dir\n{line}\n"), "capability"),
            (
                format!("{line} template-dir\ntemplate-dir\n"),
                "four tokens",
            ),
            (
                format!("{}\ntemplate-dir\n", line.replace("kvm", "none")),
                "/dev/kvm",
            ),
        ] {
            let error = version_facts(&output, &metadata).unwrap_err().to_string();
            assert!(error.contains(want), "{output:?}: {error}");
        }
    }

    #[test]
    fn facts_must_come_from_a_kvm_helper_that_restores_this_snapshot() {
        let metadata = metadata();
        let good =
            "lampyr-hluk-helper/0.1.0+hluk-df5ba0452600 hyperlight_host/0.18.0 kvm GenuineIntel\n";
        assert_eq!(
            facts(good, &metadata).unwrap(),
            good.trim_end(),
            "the line is kept whole for zygotectl -hyperlight-facts"
        );
        for (bad, want) in [
            (
                "lampyr-hluk-helper/0.1.0+hluk-df5ba0452600 kvm GenuineIntel",
                "four tokens",
            ),
            (
                "lampyr-hluk-helper/0.1.0+hluk-000000000000 hyperlight_host/0.18.0 kvm GenuineIntel",
                "does not restore",
            ),
            (
                "fiberd-hyperlight-helper/0.3.0 hyperlight_host/0.18.0 kvm GenuineIntel",
                "does not restore",
            ),
            (
                "lampyr-hluk-helper/0.1.0+hluk-df5ba0452600 hyperlight_host/0.18.0 none GenuineIntel",
                "/dev/kvm",
            ),
            (
                "lampyr-hluk-helper/0.1.0+hluk-df5ba0452600 hyperlight_host/0.18.0 kvm AuthenticAMD",
                "snapshot was made on",
            ),
        ] {
            let error = facts(bad, &metadata).unwrap_err().to_string();
            assert!(error.contains(want), "{bad}: {error}");
        }
        // An empty commit ends every helper name, and slicing a non-ASCII
        // one could panic.
        for commit in [
            "",
            "0123456789ab",
            "ééééééééééééééééééééé",
            &COMMIT.to_uppercase(),
        ] {
            let mut metadata = metadata.clone();
            metadata
                .runtime
                .insert("hyperlightUnikraft".to_owned(), commit.to_owned());
            let error = facts(good, &metadata).unwrap_err().to_string();
            assert!(error.contains("not a full commit"), "{commit:?}: {error}");
        }
    }

    #[test]
    fn recorded_facts_are_optional_and_rechecked() {
        let mut metadata = metadata();
        assert_eq!(recorded_facts(&metadata).unwrap(), None);
        let line =
            "lampyr-hluk-helper/0.3.0+hluk-df5ba0452600 hyperlight_host/0.19.0 kvm GenuineIntel";
        record_facts(&mut metadata, &format!("{line}\ntemplate-dir\n")).unwrap();
        assert_eq!(recorded_facts(&metadata).unwrap().as_deref(), Some(line));
        for tampered in [
            "tampered".to_owned(),
            format!("{line}\n"),
            format!(" {line}"),
        ] {
            let mut metadata = metadata.clone();
            metadata
                .runtime
                .insert(RUNTIME_FACTS.to_owned(), tampered.clone());
            assert!(recorded_facts(&metadata).is_err(), "{tampered:?}");
        }
    }

    #[test]
    fn helper_facts_are_part_of_the_revision_identity() {
        // The fixture sets its commit without recomputing the ID.
        let mut without = metadata();
        without
            .record_runtime("hyperlightUnikraft", COMMIT.to_owned())
            .unwrap();
        let record = |line: &str| {
            let mut metadata = without.clone();
            record_facts(&mut metadata, &format!("{line}\ntemplate-dir\n")).unwrap();
            metadata
        };
        let older = record(
            "lampyr-hluk-helper/0.3.0+hluk-df5ba0452600 hyperlight_host/0.19.0 kvm GenuineIntel",
        );
        let newer = record(
            "lampyr-hluk-helper/0.4.0+hluk-df5ba0452600 hyperlight_host/0.19.0 kvm GenuineIntel",
        );
        assert_ne!(older.revision_id, without.revision_id);
        assert_ne!(newer.revision_id, older.revision_id);
        assert_eq!(
            older.revision_id,
            record(
                "lampyr-hluk-helper/0.3.0+hluk-df5ba0452600 hyperlight_host/0.19.0 kvm GenuineIntel"
            )
            .revision_id,
            "the same helper rebuilds the same Revision"
        );
        // ly deploy and the Go controllers verify the ID over the whole
        // runtime map, so the facts cannot be edited without the ID.
        let artifact = tempfile::tempdir().unwrap();
        std::fs::write(artifact.path().join("kernel"), "kernel").unwrap();
        std::fs::create_dir(artifact.path().join("snapshot")).unwrap();
        let mut verified = older.clone();
        verified.artifacts.insert(
            "kernel".to_owned(),
            image::file_digest(&artifact.path().join("kernel")).unwrap(),
        );
        verified.artifacts.insert(
            "snapshot".to_owned(),
            image::directory_digest(&artifact.path().join("snapshot"), &[]).unwrap(),
        );
        verified
            .record_runtime(RUNTIME_FACTS, older.runtime[RUNTIME_FACTS].clone())
            .unwrap();
        let write = |metadata: &serde_json::Value| {
            std::fs::write(
                artifact.path().join("metadata.json"),
                serde_json::to_vec(metadata).unwrap(),
            )
            .unwrap()
        };
        let mut value = serde_json::to_value(&verified).unwrap();
        write(&value);
        image::verify(artifact.path()).unwrap();
        value["runtime"][RUNTIME_FACTS] = newer.runtime[RUNTIME_FACTS].clone().into();
        write(&value);
        let error = image::verify(artifact.path()).err().unwrap().to_string();
        assert!(error.contains("revision ID does not match"), "{error}");
    }

    #[test]
    fn the_tree_is_what_the_helper_loads_and_nothing_else() {
        let artifact = tempfile::tempdir().unwrap();
        let root = artifact.path();
        std::fs::write(root.join("metadata.json"), "{}").unwrap();
        std::fs::write(root.join("kernel"), "kernel").unwrap();
        std::fs::write(root.join(".lampyr-artifact"), "marker").unwrap();
        std::fs::create_dir_all(root.join("snapshot/blobs/sha256")).unwrap();
        std::fs::write(root.join("snapshot/index.json"), "index").unwrap();
        std::fs::write(root.join("snapshot/blobs/sha256/aa"), "blob").unwrap();
        let out = tempfile::tempdir().unwrap();
        let tree = out.path().join("template");
        assemble(root, &tree).unwrap();
        let mut names: Vec<String> = Vec::new();
        for entry in walk(&tree) {
            names.push(entry.strip_prefix(&tree).unwrap().display().to_string());
        }
        names.sort();
        assert_eq!(
            names,
            [
                "metadata.json",
                "snapshot",
                "snapshot/blobs",
                "snapshot/blobs/sha256",
                "snapshot/blobs/sha256/aa",
                "snapshot/index.json",
            ]
        );
        assert_eq!(
            image::directory_digest(&tree.join("snapshot"), &[]).unwrap(),
            image::directory_digest(&root.join("snapshot"), &[]).unwrap()
        );
        assert!(
            assemble(root, &tree).is_err(),
            "an existing tree is never reused"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_snapshot_with_a_symlink_is_refused() {
        let artifact = tempfile::tempdir().unwrap();
        let root = artifact.path();
        std::fs::write(root.join("metadata.json"), "{}").unwrap();
        std::fs::create_dir(root.join("snapshot")).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", root.join("snapshot/index.json")).unwrap();
        let out = tempfile::tempdir().unwrap();
        let error = assemble(root, &out.path().join("t"))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("neither a directory nor a regular file"),
            "{error}"
        );
    }

    fn walk(directory: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        for entry in std::fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                found.extend(walk(&path));
            }
            found.push(path);
        }
        found
    }

    #[test]
    fn the_annotation_round_trips_through_a_service_and_its_revision() {
        let reference = Reference {
            repository: "registry.example/team/demo".to_owned(),
            digest: format!("sha256:{}", "cd".repeat(32)),
        };
        let mut service = json!({"spec":{"template":{"metadata":{"annotations":{}}}}});
        annotate(&mut service, &reference).unwrap();
        assert_eq!(
            service["spec"]["template"]["metadata"]["annotations"][ANNOTATION],
            format!("registry.example/team/demo@sha256:{}", "cd".repeat(32))
        );
        let revision = json!({"metadata": service["spec"]["template"]["metadata"].clone()});
        assert_eq!(recorded(&revision).unwrap(), Some(reference));
        assert_eq!(
            recorded(&json!({"metadata":{"annotations":{}}})).unwrap(),
            None
        );
        for bad in [
            "registry.example/demo:tag",
            "registry.example/demo@sha256:short",
            "http://localhost:5001/demo@sha256:".to_owned().as_str(),
            "@sha256:0000000000000000000000000000000000000000000000000000000000000000",
        ] {
            assert!(Reference::parse(bad).is_err(), "{bad}");
        }
        assert!(
            recorded(&json!({"metadata":{"annotations":{ANNOTATION:"nope"}}})).is_err(),
            "a malformed annotation is an error, not an absent template"
        );
    }

    #[test]
    fn templates_go_to_the_installation_s_repository_under_a_revision_tag() {
        assert_eq!(
            push_reference(
                "registry.example/lampyr/fiberd-templates",
                "demo-0123456789ab"
            )
            .unwrap(),
            "registry.example/lampyr/fiberd-templates:demo-0123456789ab"
        );
        assert_eq!(
            push_reference(
                "localhost:5001/lampyr/fiberd-templates",
                "demo-0123456789ab"
            )
            .unwrap(),
            "http://localhost:5001/lampyr/fiberd-templates:demo-0123456789ab",
            "loopback registries are plain HTTP, as Docker treats them"
        );
        assert_eq!(
            push_reference("127.0.0.1:5001/templates", "demo-0123456789ab").unwrap(),
            "http://127.0.0.1:5001/templates:demo-0123456789ab"
        );
        // The same loopback names the install counts as local registries.
        for registry in ["[::1]", "[::1]:5001", "localhost"] {
            assert_eq!(
                push_reference(&format!("{registry}/templates"), "demo-0123456789ab").unwrap(),
                format!("http://{registry}/templates:demo-0123456789ab"),
                "{registry}"
            );
        }
        assert_eq!(
            push_reference("localhost.example.com/templates", "demo-0123456789ab").unwrap(),
            "localhost.example.com/templates:demo-0123456789ab"
        );
        assert_eq!(
            push_reference("http://reg:5000/templates", "demo-0123456789ab").unwrap(),
            "http://reg:5000/templates:demo-0123456789ab",
            "a repository the install was given as http:// is plain HTTP"
        );
        assert!(push_reference("registry.example/templates", "").is_err());
        assert!(push_reference("registry.example/templates", "demo@sha256").is_err());
        assert!(push_reference("lampyr.local/templates", "demo-0123456789ab").is_err());
    }

    #[test]
    fn deploy_seals_to_the_secret_s_public_key() {
        let public = r#"{"kty":"OKP","crv":"X25519","x":"eA","kid":"w1"}"#;
        let encoded = STANDARD.encode(public);
        let configmap = |data: &str| json!({"data": {FLEET_PUB_DATA: data}});
        assert_eq!(fleet_key(None, None).unwrap(), FleetKey::Create);
        assert_eq!(
            fleet_key(Some(Some(&encoded)), None).unwrap(),
            FleetKey::Restore(public.as_bytes().to_vec())
        );
        assert_eq!(
            fleet_key(Some(Some(&encoded)), Some(&configmap(public))).unwrap(),
            FleetKey::Ready(public.as_bytes().to_vec())
        );
        let other = r#"{"kty":"OKP","crv":"X25519","x":"other","kid":"w2"}"#;
        let error = fleet_key(Some(Some(&encoded)), Some(&configmap(other)))
            .unwrap_err()
            .to_string();
        assert!(error.contains("differs"), "{error}");
        assert!(
            fleet_key(Some(Some(&encoded)), Some(&json!({"data": {}}))).is_err(),
            "a ConfigMap without the key is no match"
        );
        let error = fleet_key(None, Some(&configmap(public)))
            .unwrap_err()
            .to_string();
        assert!(error.contains("exists without"), "{error}");
        assert!(fleet_key(Some(None), None).is_err());
        let private =
            STANDARD.encode(r#"{"kty":"OKP","crv":"X25519","x":"eA","d":"cA","kid":"w1"}"#);
        assert!(
            fleet_key(Some(Some(&private)), None).is_err(),
            "a private key is never published"
        );
        // kubectl prints the Secret's name, a space and its public key.
        assert_eq!(parse_secret_public(""), None);
        assert_eq!(parse_secret_public(SEAL_KEY_SECRET), Some(None));
        assert_eq!(
            parse_secret_public(&format!("{SEAL_KEY_SECRET} {encoded}")),
            Some(Some(encoded.clone()))
        );
    }

    #[test]
    fn key_resources_keep_the_private_half_in_the_grant_pods_secret() {
        let seal = br#"{"use":"enc","kty":"oct","kid":"k1","alg":"A256GCM","k":"c2VjcmV0"}"#;
        let public =
            br#"{"kty":"OKP","crv":"X25519","x":"eA","kid":"w1","use":"enc","alg":"ECDH-ES"}"#;
        let (secret, configmap) = key_resources(seal, public).unwrap();
        assert_eq!(secret["metadata"]["name"], "lampyr-fiberd-seal-key");
        assert_eq!(secret["metadata"]["namespace"], "default");
        assert_eq!(
            STANDARD
                .decode(secret["data"]["seal-key.json"].as_str().unwrap())
                .unwrap(),
            seal
        );
        assert_eq!(
            STANDARD
                .decode(secret["data"]["fleet-pub.json"].as_str().unwrap())
                .unwrap(),
            public
        );
        assert_eq!(configmap["metadata"]["name"], "lampyr-fiberd-fleet-pub");
        assert_eq!(configmap["metadata"]["namespace"], SYSTEM);
        assert_eq!(
            configmap["data"]["fleet-pub.json"]
                .as_str()
                .unwrap()
                .as_bytes(),
            public
        );
        assert!(
            !configmap.to_string().contains("c2VjcmV0"),
            "the ConfigMap never carries the private key"
        );
        assert!(
            key_resources(public, public).is_err(),
            "a public key is not a seal key"
        );
        let leaked = br#"{"kty":"OKP","crv":"X25519","x":"eA","d":"cHJpdg","kid":"w1"}"#;
        assert!(
            key_resources(seal, leaked).is_err(),
            "a private X25519 key is never published"
        );
    }
}
