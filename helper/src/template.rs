// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

//! The template the helper warms: a Lampyr artifact directory, as `ly build`
//! writes it, holding `metadata.json` and the hluk snapshot in `snapshot/`.
//!
//! fiberd hands the directory over read-only, and the helper never writes
//! into it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::facts;

const SCHEMA: &str = "lampyr.dev/artifact/v1alpha2";

/// A loaded template: where its snapshot is and what runs from it.
#[derive(Debug)]
pub struct Template {
    pub snapshot: PathBuf,
    /// The guest command the app starts with, after each clone's restore.
    pub command: String,
    /// The project's service bindings, binding name to Service.
    pub bindings: BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Metadata {
    schema_version: String,
    project: Project,
    runtime: Runtime,
}

#[derive(Deserialize)]
struct Project {
    command: String,
    #[serde(default)]
    bindings: BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Runtime {
    cpu_vendor: String,
    hyperlight_unikraft: String,
}

impl Template {
    /// Read the template in `dir`. A snapshot made by another
    /// hyperlight-unikraft commit, or on another CPU vendor, cannot be
    /// restored here, so it is refused now rather than inside a clone.
    pub fn load(dir: &Path) -> Result<Self> {
        let path = dir.join("metadata.json");
        let text =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let metadata: Metadata =
            serde_json::from_str(&text).with_context(|| format!("decode {}", path.display()))?;
        if metadata.schema_version != SCHEMA {
            bail!(
                "template schema {:?} is not {SCHEMA:?}",
                metadata.schema_version
            );
        }
        if metadata.runtime.hyperlight_unikraft != facts::HYPERLIGHT_UNIKRAFT_COMMIT {
            bail!(
                "template was snapshotted by hyperlight-unikraft {}, this helper restores {}: rebuild it with a matching ly",
                metadata.runtime.hyperlight_unikraft,
                facts::HYPERLIGHT_UNIKRAFT_COMMIT
            );
        }
        let cpu = facts::cpu_vendor();
        if metadata.runtime.cpu_vendor != cpu {
            bail!(
                "template was snapshotted on {}, this host is {cpu}",
                metadata.runtime.cpu_vendor
            );
        }
        if metadata.project.command.trim().is_empty() {
            bail!("template has no command");
        }
        let snapshot = dir.join("snapshot");
        if !snapshot.is_dir() {
            bail!("{} is not a directory", snapshot.display());
        }
        Ok(Template {
            snapshot,
            command: metadata.project.command,
            bindings: metadata.project.bindings,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn template(metadata: serde_json::Value, snapshot: bool) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("metadata.json"), metadata.to_string()).unwrap();
        if snapshot {
            std::fs::create_dir(dir.path().join("snapshot")).unwrap();
        }
        dir
    }

    fn metadata(commit: &str, cpu: &str, command: &str) -> serde_json::Value {
        serde_json::json!({
            "schemaVersion": SCHEMA,
            "project": { "command": command },
            "runtime": { "cpuVendor": cpu, "hyperlightUnikraft": commit },
        })
    }

    #[test]
    fn a_matching_template_loads() {
        let dir = template(
            metadata(
                facts::HYPERLIGHT_UNIKRAFT_COMMIT,
                &facts::cpu_vendor(),
                "/entrypoint",
            ),
            true,
        );
        let t = Template::load(dir.path()).unwrap();
        assert_eq!(t.command, "/entrypoint");
        assert_eq!(t.snapshot, dir.path().join("snapshot"));
        assert!(t.bindings.is_empty());
    }

    #[test]
    fn a_template_carries_its_project_s_bindings() {
        let mut value = metadata(
            facts::HYPERLIGHT_UNIKRAFT_COMMIT,
            &facts::cpu_vendor(),
            "/entrypoint",
        );
        value["project"]["bindings"] = serde_json::json!({"processor": "worker-processor"});
        let dir = template(value, true);
        let t = Template::load(dir.path()).unwrap();
        assert_eq!(
            t.bindings,
            BTreeMap::from([("processor".to_owned(), "worker-processor".to_owned())])
        );
    }

    #[test]
    fn a_template_this_helper_cannot_restore_is_refused() {
        let commit = facts::HYPERLIGHT_UNIKRAFT_COMMIT;
        let cpu = facts::cpu_vendor();
        for (name, value, snapshot, want) in [
            (
                "another hluk",
                metadata("0000", &cpu, "/entrypoint"),
                true,
                "rebuild",
            ),
            (
                "another CPU vendor",
                metadata(commit, "SomeOtherCPU", "/entrypoint"),
                true,
                "this host is",
            ),
            (
                "no command",
                metadata(commit, &cpu, " "),
                true,
                "no command",
            ),
            (
                "no snapshot",
                metadata(commit, &cpu, "/entrypoint"),
                false,
                "not a directory",
            ),
            (
                "another schema",
                serde_json::json!({ "schemaVersion": "lampyr.dev/artifact/v0", "project": { "command": "/e" },
                    "runtime": { "cpuVendor": cpu, "hyperlightUnikraft": commit } }),
                true,
                "schema",
            ),
        ] {
            let dir = template(value, snapshot);
            let err = Template::load(dir.path()).unwrap_err().to_string();
            assert!(err.contains(want), "{name}: {err}");
        }
    }
}
