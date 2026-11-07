// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

macro_rules! example_files {
    ($runtime:literal, $($name:literal),+ $(,)?) => {{
        Template {
            manifest: include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"), "/../templates/services/", $runtime, "/manifest.json"
            )),
            files: &[$(($name, include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"), "/../templates/services/", $runtime, "/", $name
            )))),+],
        }
    }};
}

struct Template {
    manifest: &'static str,
    files: &'static [(&'static str, &'static [u8])],
}

pub const ADAPTER_SCHEMA: &str = "lampyr.dev/runtime-adapter/v1alpha1";
pub const TEMPLATE_SCHEMA: &str = "lampyr.dev/service-template/v1alpha1";
pub(crate) const MISSING_APPLICATION: &str = "application.type must be handler or standalone; add `application: {type: handler}` to lampyr.yaml for a Runtime Adapter service, or `application: {type: standalone, command: ...}` for a customer-owned server";

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TemplateManifest {
    pub schema_version: String,
    pub runtime: String,
    pub template_version: String,
    pub entrypoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_warmup: Option<String>,
    pub user_owned: Vec<String>,
    pub adapter_owned: Vec<String>,
    pub machine_state: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AdapterMetadata {
    pub schema_version: String,
    pub runtime: String,
    pub template_version: String,
    pub entrypoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_warmup: Option<String>,
    /// SHA-256 digests of the pristine rendered files, relative to the adapter directory.
    #[serde(deserialize_with = "unique_adapter_files")]
    pub files: BTreeMap<String, String>,
    /// The template's own entrypoint and warmup, so upgrades can tell customer
    /// edits of the fields above from template changes (three-way merge base).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pristine: Option<PristineMetadata>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PristineMetadata {
    pub entrypoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_warmup: Option<String>,
}

impl AdapterMetadata {
    /// The recorded template values, or the adapter's own values when metadata
    /// predates `pristine` (upgrades resolve that case against the template).
    pub fn pristine_values(&self) -> PristineMetadata {
        self.pristine.clone().unwrap_or_else(|| self.own_values())
    }

    pub fn own_values(&self) -> PristineMetadata {
        PristineMetadata {
            entrypoint: self.entrypoint.clone(),
            snapshot_warmup: self.snapshot_warmup.clone(),
        }
    }
}

fn unique_adapter_files<'de, D>(
    deserializer: D,
) -> std::result::Result<BTreeMap<String, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct Files;
    impl<'de> serde::de::Visitor<'de> for Files {
        type Value = BTreeMap<String, String>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("unique adapter paths and pristine digests")
        }

        fn visit_map<M: serde::de::MapAccess<'de>>(
            self,
            mut map: M,
        ) -> std::result::Result<Self::Value, M::Error> {
            let mut files = BTreeMap::new();
            while let Some((path, digest)) = map.next_entry::<String, String>()? {
                if files.insert(path.clone(), digest).is_some() {
                    return Err(serde::de::Error::custom(format!(
                        "duplicate adapter path {path:?}"
                    )));
                }
            }
            Ok(files)
        }
    }
    deserializer.deserialize_map(Files)
}

fn template(runtime: &str) -> Option<Template> {
    Some(match runtime {
        "nodejs" => example_files!(
            "nodejs",
            "project/.gitignore",
            "project/Dockerfile",
            "adapter/Dockerfile.rootfs",
            "adapter/package.json",
            "adapter/package-lock.json",
            "project/handler.js",
            "project/lampyr.yaml",
            "adapter/server.js",
            "adapter/types.js",
            "adapter/types.d.ts",
        ),
        "python" => example_files!(
            "python",
            "project/.gitignore",
            "project/Dockerfile",
            "adapter/Dockerfile.rootfs",
            "project/handler.py",
            "project/lampyr.yaml",
            "adapter/server.py",
            "adapter/lampyr_types.py",
            "adapter/requirements.in",
            "adapter/requirements.txt",
        ),
        "go" => example_files!(
            "go",
            "project/.gitignore",
            "project/Dockerfile",
            "adapter/Dockerfile.rootfs",
            "project/lampyr.yaml",
            "project/handler.go",
            "adapter/server.go",
            "adapter/types.go",
            "adapter/go.mod",
            "adapter/go.sum",
        ),
        "rust" => example_files!(
            "rust",
            "project/.gitignore",
            "project/Dockerfile",
            "adapter/Dockerfile.rootfs",
            "project/lampyr.yaml",
            "project/handler.rs",
            "adapter/server.rs",
            "adapter/types.rs",
            "adapter/Cargo.toml",
            "adapter/Cargo.lock",
        ),
        "dotnet" => example_files!(
            "dotnet",
            "project/.gitignore",
            "project/Dockerfile",
            "adapter/Dockerfile.rootfs",
            "project/Handler.cs",
            "adapter/LampyrService.csproj",
            "adapter/Program.cs",
            "adapter/Types.cs",
            "project/lampyr.yaml",
        ),
        _ => return None,
    })
}

fn standalone_template(runtime: &str) -> Option<Template> {
    Some(match runtime {
        "nodejs" => example_files!(
            "nodejs/standalone",
            "project/.gitignore",
            "project/Dockerfile",
            "project/Dockerfile.rootfs",
            "project/lampyr.yaml",
            "project/package.json",
            "project/package-lock.json",
            "project/application.js",
            "project/server.js",
        ),
        "python" => example_files!(
            "python/standalone",
            "project/.gitignore",
            "project/Dockerfile",
            "project/Dockerfile.rootfs",
            "project/lampyr.yaml",
            "project/requirements.txt",
            "project/application.py",
            "project/server.py",
        ),
        "go" => example_files!(
            "go/standalone",
            "project/.gitignore",
            "project/Dockerfile",
            "project/Dockerfile.rootfs",
            "project/lampyr.yaml",
            "project/go.mod",
            "project/go.sum",
            "project/application.go",
            "project/main.go",
        ),
        "rust" => example_files!(
            "rust/standalone",
            "project/.gitignore",
            "project/Dockerfile",
            "project/Dockerfile.rootfs",
            "project/lampyr.yaml",
            "project/Cargo.toml",
            "project/Cargo.lock",
            "project/src/application.rs",
            "project/src/main.rs",
        ),
        "dotnet" => example_files!(
            "dotnet/standalone",
            "project/.gitignore",
            "project/Dockerfile",
            "project/Dockerfile.rootfs",
            "project/lampyr.yaml",
            "project/Service.csproj",
            "project/Application.cs",
            "project/Program.cs",
        ),
        _ => return None,
    })
}

impl Template {
    fn render(&self, runtime: &str, name: &str) -> Result<BTreeMap<String, Vec<u8>>> {
        let manifest: TemplateManifest =
            serde_json::from_str(self.manifest).context("decode service template manifest")?;
        ensure!(
            manifest.schema_version == TEMPLATE_SCHEMA && manifest.runtime == runtime,
            "service template schema or runtime does not match"
        );
        ensure!(
            !manifest.template_version.is_empty() && manifest.entrypoint.starts_with('/'),
            "service template version and absolute entrypoint are required"
        );
        ensure!(
            manifest.machine_state == [".lampyr/"],
            "service template machine state must be .lampyr/"
        );
        let sources: BTreeMap<_, _> = self.files.iter().copied().collect();
        ensure!(
            sources.len() == self.files.len(),
            "duplicate service template source"
        );
        let mut used = BTreeSet::new();
        let mut files = BTreeMap::new();
        let mut hashes = BTreeMap::new();
        let original = format!("hello-{runtime}");
        for (ownership, names) in [
            ("project", &manifest.user_owned),
            ("adapter", &manifest.adapter_owned),
        ] {
            for filename in names {
                let path = Path::new(filename);
                ensure!(
                    !filename.is_empty()
                        && !filename.contains('\\')
                        && path
                            .components()
                            .all(|part| matches!(part, std::path::Component::Normal(_)))
                        && !path.starts_with(".lampyr")
                        && !path.starts_with(".lampyr-adapter")
                        && (ownership != "adapter"
                            || (!path.starts_with("baseline")
                                && !path.starts_with("pending-baseline")
                                && filename != "adapter.json"
                                && filename != "pending.json")),
                    "invalid or reserved service template path {filename:?}"
                );
                let source_name = format!("{ownership}/{filename}");
                ensure!(
                    used.insert(source_name.clone()),
                    "duplicate ownership for {source_name}"
                );
                let source = sources
                    .get(source_name.as_str())
                    .with_context(|| format!("missing template source {source_name}"))?;
                let contents = match std::str::from_utf8(source) {
                    Ok(text) => text.replace(&original, name).into_bytes(),
                    Err(_) => source.to_vec(),
                };
                if ownership == "adapter" {
                    hashes.insert(
                        filename.clone(),
                        format!(
                            "sha256:{}",
                            crate::image::hex(Sha256::digest(&contents).as_slice())
                        ),
                    );
                    files.insert(
                        format!(".lampyr-adapter/baseline/{filename}"),
                        contents.clone(),
                    );
                    files.insert(format!(".lampyr-adapter/{filename}"), contents);
                } else {
                    files.insert(filename.clone(), contents);
                }
            }
        }
        ensure!(
            used.len() == sources.len(),
            "service template contains unclassified sources"
        );
        if !hashes.is_empty() {
            let metadata = AdapterMetadata {
                schema_version: ADAPTER_SCHEMA.to_owned(),
                runtime: manifest.runtime,
                template_version: manifest.template_version,
                pristine: Some(PristineMetadata {
                    entrypoint: manifest.entrypoint.clone(),
                    snapshot_warmup: manifest.snapshot_warmup.clone(),
                }),
                entrypoint: manifest.entrypoint,
                snapshot_warmup: manifest.snapshot_warmup,
                files: hashes,
            };
            let mut encoded = serde_json::to_vec_pretty(&metadata)?;
            encoded.push(b'\n');
            files.insert(".lampyr-adapter/adapter.json".to_owned(), encoded);
        }
        for filename in files.keys() {
            ensure!(
                !Path::new(filename)
                    .ancestors()
                    .skip(1)
                    .any(|parent| files.contains_key(parent.to_str().unwrap_or(""))),
                "service template file/directory collision at {filename}"
            );
        }
        Ok(files)
    }
}

pub(crate) fn rendered_template(runtime: &str, name: &str) -> Result<BTreeMap<String, Vec<u8>>> {
    template(runtime)
        .with_context(|| format!("runtime {runtime:?} is not supported"))?
        .render(runtime, name)
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Project {
    pub api_version: String,
    pub kind: String,
    pub name: String,
    pub runtime: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub application: Option<Application>,
    pub command: String,
    #[serde(skip)]
    pub(crate) adapter_managed: bool,
    #[serde(skip)]
    pub(crate) snapshot_warmup: Option<String>,
    pub protocols: Protocols,
    pub events: Events,
    pub resources: Resources,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution: Option<Execution>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub bindings: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum Application {
    Handler {},
    Standalone { command: String },
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Protocols {
    pub http: bool,
    pub streaming: bool,
    pub websocket: bool,
    pub events: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub grpc: bool,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Events {
    pub max_retries: i32,
    pub initial_backoff: String,
    pub attempt_timeout: String,
    pub dead_letter: String,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Resources {
    pub memory: String,
    pub cpu: String,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Execution {
    pub mode: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub idle_timeout: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub idle_action: String,
}

impl Project {
    pub fn load(directory: impl AsRef<Path>) -> Result<Self> {
        let directory = directory.as_ref();
        let source = std::fs::read(directory.join("lampyr.yaml")).context("read lampyr.yaml")?;
        let mut project: Self = serde_yaml_ng::from_slice(&source).context("decode lampyr.yaml")?;
        let manifest: serde_yaml_ng::Value = serde_yaml_ng::from_slice(&source)?;
        ensure!(project.application.is_some(), MISSING_APPLICATION);
        validate_runtime(&project.runtime)?;
        if let Some(application) = &project.application {
            ensure!(
                manifest.get("command").is_none() && manifest.get("protocols").is_none(),
                "application manifests must omit top-level command and protocols"
            );
            if let Application::Standalone { command } = application {
                match std::fs::symlink_metadata(directory.join(".lampyr-adapter")) {
                    Ok(_) => bail!("standalone applications must not contain .lampyr-adapter"),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error).context("inspect standalone adapter path"),
                }
                project.command = command.clone();
            }
        }
        if matches!(project.application, Some(Application::Handler {})) {
            let directory = directory
                .canonicalize()
                .context("resolve service directory")?;
            let metadata = crate::adapter::metadata(&directory, &project.runtime)
                .context("command is required with valid runtime adapter metadata")?;
            ensure!(
                matches!(
                    project.runtime.as_str(),
                    "nodejs" | "python" | "go" | "rust" | "dotnet"
                ),
                "runtime requires an explicit command and protocols in this template version"
            );
            project.command = metadata.entrypoint;
            project.snapshot_warmup = metadata.snapshot_warmup;
            project.adapter_managed = true;
        }
        if project.application.is_some() || project.adapter_managed {
            project.protocols = Protocols {
                http: true,
                streaming: true,
                websocket: true,
                events: manifest.get("events").is_some(),
                grpc: false,
            };
        }
        project.validate()?;
        Ok(project)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.api_version == "lampyr.dev/v1alpha1",
            "apiVersion must be \"lampyr.dev/v1alpha1\""
        );
        ensure!(self.kind == "Project", "kind must be \"Project\"");
        ensure!(!self.name.is_empty(), "name is required");
        validate_runtime(&self.runtime)?;
        ensure!(!self.command.trim().is_empty(), "command is required");
        for (binding, service) in &self.bindings {
            ensure!(
                !binding.is_empty()
                    && binding
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
                    && binding.as_bytes()[0].is_ascii_lowercase(),
                "binding names must start with a lowercase letter and contain only a-z, 0-9 and _"
            );
            validate_name(service)?;
        }
        if let Some(application) = &self.application {
            if let Application::Standalone { command } = application {
                ensure!(
                    !command.is_empty() && command.trim() == command && *command == self.command,
                    "standalone application.command must be nonempty without surrounding whitespace and match the effective command"
                );
            }
            ensure!(
                self.protocols.http
                    && self.protocols.streaming
                    && self.protocols.websocket
                    && !self.protocols.grpc,
                "application models require HTTP, streaming and WebSockets without capability flags"
            );
        }
        ensure!(
            !self.resources.memory.is_empty(),
            "resources.memory is required"
        );
        ensure!(!self.resources.cpu.is_empty(), "resources.cpu is required");
        ensure!(
            !self.protocols.grpc || !self.protocols.websocket,
            "protocols.grpc cannot be combined with protocols.websocket"
        );
        if self.protocols.events {
            ensure!(
                (0..=10).contains(&self.events.max_retries),
                "events.maxRetries must be between 0 and 10"
            );
            ensure!(
                !self.events.initial_backoff.is_empty(),
                "events.initialBackoff is required"
            );
            ensure!(
                !self.events.dead_letter.is_empty(),
                "events.deadLetter is required"
            );
        }
        self.effective_execution()?;
        Ok(())
    }

    pub fn application_model(&self) -> &'static str {
        match self.application {
            Some(Application::Handler {}) => "handler",
            Some(Application::Standalone { .. }) => "standalone",
            None if self.adapter_managed => "handler",
            None => "unknown",
        }
    }

    pub fn effective_execution(&self) -> Result<Execution> {
        let mut execution = self.execution.clone().unwrap_or_default();
        if execution.mode.is_empty() {
            execution.mode = "fresh-per-request".to_owned();
        }
        match execution.mode.as_str() {
            "fresh-per-request" => {
                ensure!(
                    execution.idle_timeout.is_empty() && execution.idle_action.is_empty(),
                    "execution idle settings are only valid with mode reuse"
                );
            }
            "reuse" => {
                if execution.idle_timeout.is_empty() {
                    execution.idle_timeout = "30s".to_owned();
                }
                if execution.idle_action.is_empty() {
                    execution.idle_action = "release".to_owned();
                }
                ensure!(
                    execution.idle_action == "release",
                    "execution.idleAction must be release; park/resume is not supported"
                );
                ensure!(
                    go_parse_duration::parse_duration(&execution.idle_timeout)
                        .is_ok_and(|duration| duration > 0),
                    "execution.idleTimeout must be a positive duration"
                );
            }
            _ => bail!(
                "execution.mode must be \"fresh-per-request\" or \"reuse\"; park/resume is not supported"
            ),
        }
        Ok(execution)
    }
}

pub fn initialize(directory: &Path, name: Option<&str>, runtime: &str) -> Result<()> {
    initialize_application(directory, name, runtime, false)
}

pub fn initialize_application(
    directory: &Path,
    name: Option<&str>,
    runtime: &str,
    standalone: bool,
) -> Result<()> {
    let absolute = std::path::absolute(directory)?;
    let name = name
        .unwrap_or(
            absolute
                .file_name()
                .and_then(|name| name.to_str())
                .context("project name is required")?,
        )
        .trim();
    validate_service_name(name)?;
    let template = if standalone {
        standalone_template(runtime)
            .with_context(|| format!("standalone runtime {runtime:?} is not supported"))?
    } else {
        template(runtime).with_context(|| format!("runtime {runtime:?} is not supported"))?
    };
    let files = template.render(runtime, name)?;
    let adapter = ".lampyr-adapter";
    let destinations = std::iter::once(adapter).chain(files.keys().map(String::as_str));
    for filename in destinations {
        let destination = absolute.join(filename);
        match std::fs::symlink_metadata(&destination) {
            Ok(_) => bail!("{filename} already exists"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("check project destination"),
        }
        for parent in destination
            .ancestors()
            .skip(1)
            .take_while(|p| *p != absolute)
        {
            match std::fs::symlink_metadata(parent) {
                Ok(metadata) => ensure!(
                    metadata.is_dir(),
                    "project destination parent must be a real directory: {}",
                    parent.display()
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("check project destination parent"),
            }
        }
    }
    std::fs::create_dir_all(&absolute)?;
    for (filename, contents) in files {
        let path = absolute.join(filename);
        std::fs::create_dir_all(path.parent().context("project file has no parent")?)?;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| format!("create {} without overwriting", path.display()))?;
        file.write_all(&contents)?;
    }
    Ok(())
}

pub fn validate_runtime(runtime: &str) -> Result<()> {
    ensure!(
        matches!(runtime, "nodejs" | "python" | "go" | "rust" | "dotnet"),
        "runtime {runtime:?} is not supported"
    );
    Ok(())
}

/// The `-<12 hex>` of the artifact's Revision ID that ends each Revision name.
pub const REVISION_SUFFIX_LENGTH: usize = 13;
/// fiberd-k8s-controller caps a CapacityGrant's name, which is its
/// Revision's, at 56 characters so that the grant's `fiberd-<grant>`
/// Service still fits a DNS label.
pub const MAX_SERVICE_NAME: usize = 56 - REVISION_SUFFIX_LENGTH;

/// Refuses a service name whose Revisions could never get a grant, so a
/// project fails at scaffold or build rather than after deploy publishes.
pub fn validate_service_name(name: &str) -> Result<()> {
    validate_name(name)?;
    ensure!(
        name.len() <= MAX_SERVICE_NAME,
        "service name {name:?} is {} characters; it must be at most {MAX_SERVICE_NAME} so its Revisions can name a CapacityGrant",
        name.len()
    );
    Ok(())
}

pub fn validate_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 63
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !name.starts_with('-')
            && !name.ends_with('-'),
        "service name must be a lowercase DNS label of at most 63 characters"
    );
    Ok(())
}
