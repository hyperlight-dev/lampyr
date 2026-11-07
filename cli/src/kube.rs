// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::host::Host;

pub const SYSTEM: &str = "lampyr-system";
pub const VISIBILITY_LABEL: &str = "networking.knative.dev/visibility";

pub fn cluster_local(service: &Value) -> bool {
    service["metadata"]["labels"][VISIBILITY_LABEL].as_str() == Some("cluster-local")
}

fn normalized_kubeconfig(paths: std::ffi::OsString) -> Result<std::ffi::OsString> {
    let paths = std::env::split_paths(&paths)
        .map(|path| {
            if path.as_os_str().is_empty() {
                Ok(path)
            } else {
                std::path::absolute(path)
            }
        })
        .collect::<std::io::Result<Vec<_>>>()?;
    Ok(std::env::join_paths(paths)?)
}

pub(crate) fn kubectl_command(host: &Host, directory: &Path) -> Result<Command> {
    let mut command = host.command("kubectl")?;
    if let Some(paths) = std::env::var_os("KUBECONFIG") {
        command.env("KUBECONFIG", normalized_kubeconfig(paths)?);
    }
    command.current_dir(directory);
    Ok(command)
}

pub struct Kube<'a> {
    pub host: &'a Host,
    pub directory: &'a Path,
    pub context: String,
    deadline: Cell<Option<Instant>>,
}

impl<'a> Kube<'a> {
    pub fn new(directory: &'a Path, context: Option<&str>, host: &'a Host) -> Result<Self> {
        let context = match context {
            Some(context) => {
                ensure!(!context.is_empty(), "Kubernetes context must not be empty");
                context.to_owned()
            }
            None => host
                .capture(kubectl_command(host, directory)?.args(["config", "current-context"]))
                .context("select Kubernetes context; pass --context NAME")?,
        };
        Ok(Self {
            host,
            directory,
            context,
            deadline: Cell::new(None),
        })
    }

    pub fn command(&self, namespace: Option<&str>) -> Result<Command> {
        let mut command = kubectl_command(self.host, self.directory)?;
        command.args(["--context", &self.context]);
        if let Some(namespace) = namespace {
            command.args(["--namespace", namespace]);
        }

        Ok(command)
    }

    pub fn capture(&self, command: &mut Command) -> Result<String> {
        let output = self
            .host
            // kubectl may need to refresh credentials before making its API request.
            .output_timeout(command, self.timeout(Duration::from_secs(120))?)?;
        self.host.require_success(command, &output)?;
        ensure!(!output.truncated, "Kubernetes response exceeds 8 MiB");
        Ok(output.stdout.trim().to_owned())
    }

    pub fn get<T: DeserializeOwned>(
        &self,
        namespace: &str,
        resource: &str,
        name: &str,
    ) -> Result<T> {
        let value = self.capture(
            self.command(Some(namespace))?
                .args(["get", resource, name, "--output", "json"]),
        )?;
        serde_json::from_str(&value).with_context(|| format!("decode {resource}/{name}"))
    }

    pub fn optional(&self, namespace: &str, resource: &str, name: &str) -> Result<Option<Value>> {
        let value = self.capture(self.command(Some(namespace))?.args([
            "get",
            resource,
            name,
            "--ignore-not-found=true",
            "--output",
            "json",
        ]))?;
        if value.is_empty() {
            return Ok(None);
        }
        Ok(Some(
            serde_json::from_str(&value).with_context(|| format!("decode {resource}/{name}"))?,
        ))
    }

    pub fn list<T: DeserializeOwned>(
        &self,
        namespace: &str,
        resource: &str,
        selector: &str,
    ) -> Result<Vec<T>> {
        #[derive(Deserialize)]
        struct List<T> {
            items: Vec<T>,
        }
        let value = self.capture(self.command(Some(namespace))?.args([
            "get",
            resource,
            "--selector",
            selector,
            "--output",
            "json",
        ]))?;
        let list: List<T> =
            serde_json::from_str(&value).with_context(|| format!("decode {resource} list"))?;
        Ok(list.items)
    }

    pub fn apply(&self, value: &impl Serialize) -> Result<()> {
        let mut file = tempfile::NamedTempFile::new()?;
        serde_json::to_writer(file.as_file_mut(), value)?;
        self.host.run(
            self.command(None)?
                .args(["apply", "--filename"])
                .arg(file.path()),
        )
    }

    pub fn create(&self, value: &impl Serialize) -> Result<()> {
        let mut file = tempfile::NamedTempFile::new()?;
        serde_json::to_writer(file.as_file_mut(), value)?;
        self.host.run(
            self.command(None)?
                .args(["create", "--filename"])
                .arg(file.path()),
        )
    }

    pub fn manifest_optional(&self, value: &impl Serialize) -> Result<Option<Value>> {
        let mut file = tempfile::NamedTempFile::new()?;
        serde_json::to_writer(file.as_file_mut(), value)?;
        let output = self.capture(
            self.command(None)?
                .args(["get", "--filename"])
                .arg(file.path())
                .args(["--ignore-not-found=true", "--output", "json"]),
        )?;
        if output.is_empty() {
            return Ok(None);
        }
        Ok(Some(
            serde_json::from_str(&output).context("decode Kubernetes manifest observation")?,
        ))
    }

    pub fn delete_manifest(&self, value: &impl Serialize) -> Result<()> {
        let mut file = tempfile::NamedTempFile::new()?;
        serde_json::to_writer(file.as_file_mut(), value)?;
        self.host.run(
            self.command(None)?
                .args(["delete", "--filename"])
                .arg(file.path())
                .args(["--ignore-not-found=true", "--wait=true", "--timeout=300s"]),
        )
    }

    pub fn patch(&self, namespace: &str, resource: &str, name: &str, value: &Value) -> Result<()> {
        self.capture(self.command(Some(namespace))?.args([
            "patch",
            resource,
            name,
            "--type",
            "merge",
            "--patch",
            &serde_json::to_string(value)?,
        ]))
        .map(|_| ())
    }

    pub fn installation(&self) -> Result<Installation> {
        #[derive(Deserialize)]
        struct ConfigMap {
            data: BTreeMap<String, String>,
        }
        let config: ConfigMap = self.get("kube-system", "configmap", "lampyr-install")?;
        let domain = config
            .data
            .get("domain")
            .filter(|value| !value.is_empty())
            .cloned()
            .unwrap_or_else(|| "localhost".to_owned());
        ensure!(
            domain.split('.').all(|label| !label.is_empty()
                && label.len() <= 63
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                && !label.starts_with('-')
                && !label.ends_with('-')),
            "installation has an invalid domain"
        );
        let https_port = config
            .data
            .get("httpsPort")
            .filter(|value| !value.is_empty())
            .cloned();
        if let Some(port) = &https_port {
            ensure!(
                port.parse::<u16>().is_ok_and(|value| value > 0),
                "installation has an invalid HTTPS port"
            );
        }
        Ok(Installation {
            data: config.data,
            domain,
            https_port,
        })
    }

    pub fn wait_crds(&self, names: &[&str], timeout: Duration) -> Result<()> {
        self.wait(
            &format!("CRD establishment ({})", names.join(", ")),
            timeout,
            || {
                for name in names {
                    let resource: Value = self.get("default", "crd", name)?;
                    let Some(conditions) = resource
                        .pointer("/status/conditions")
                        .filter(|value| !value.is_null())
                    else {
                        return Ok(None);
                    };
                    let conditions = conditions
                        .as_array()
                        .with_context(|| format!("CRD {name} conditions must be an array"))?;
                    if !conditions.iter().any(|condition| {
                        condition["type"] == "Established" && condition["status"] == "True"
                    }) {
                        return Ok(None);
                    }
                }
                Ok(Some(()))
            },
        )
    }

    pub fn wait<T>(
        &self,
        label: &str,
        timeout: Duration,
        mut probe: impl FnMut() -> Result<Option<T>>,
    ) -> Result<T> {
        let deadline = Instant::now() + timeout;
        struct Reset<'a>(&'a Cell<Option<Instant>>, Option<Instant>);
        impl Drop for Reset<'_> {
            fn drop(&mut self) {
                self.0.set(self.1);
            }
        }
        let previous = self.deadline.get();
        self.deadline.set(Some(
            previous.map_or(deadline, |previous| previous.min(deadline)),
        ));
        let _reset = Reset(&self.deadline, previous);
        loop {
            ensure!(!self.host.cancellation.is_cancelled(), "{label} cancelled");
            ensure!(
                Instant::now() < deadline,
                "{label} timed out after {}s",
                timeout.as_secs()
            );
            let result = probe()?;
            ensure!(!self.host.cancellation.is_cancelled(), "{label} cancelled");
            ensure!(
                Instant::now() < deadline,
                "{label} timed out after {}s",
                timeout.as_secs()
            );
            if let Some(result) = result {
                return Ok(result);
            }
            std::thread::sleep(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(250)),
            );
        }
    }

    fn timeout(&self, timeout: Duration) -> Result<Duration> {
        let timeout = self.deadline.get().map_or(timeout, |deadline| {
            timeout.min(deadline.saturating_duration_since(Instant::now()))
        });
        ensure!(!timeout.is_zero(), "Kubernetes operation deadline exceeded");
        Ok(timeout)
    }
}

pub struct Installation {
    pub data: BTreeMap<String, String>,
    pub domain: String,
    pub https_port: Option<String>,
}

impl Installation {
    pub fn required(&self, key: &str) -> Result<&str> {
        self.data
            .get(key)
            .filter(|value| !value.is_empty())
            .map(String::as_str)
            .with_context(|| format!("installation has no {key}; run ly cluster install"))
    }
}

pub fn selector(service: &str) -> String {
    format!("edge.lampyr.dev/service={service}")
}

pub fn labels(service: &str) -> Value {
    json!({ "app.kubernetes.io/managed-by": "lampyr", "edge.lampyr.dev/service": service })
}

pub fn ensure_owner(resource: &Value, service: &str) -> Result<()> {
    ensure!(
        resource
            .pointer("/metadata/labels/app.kubernetes.io~1managed-by")
            .and_then(Value::as_str)
            == Some("lampyr")
            && resource
                .pointer("/metadata/labels/edge.lampyr.dev~1service")
                .and_then(Value::as_str)
                == Some(service),
        "refusing to change a resource not owned by this Lampyr Service"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kubeconfig_entries_are_absolutized_before_command_directory_changes() {
        let paths = std::env::join_paths(["first", "second"]).unwrap();
        let normalized = normalized_kubeconfig(paths).unwrap();
        let entries: Vec<_> = std::env::split_paths(&normalized).collect();
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|path| path.is_absolute()));
        assert!(entries[0].ends_with("first"));
        assert!(entries[1].ends_with("second"));
    }
}
