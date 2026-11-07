// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

pub struct Profile<'a> {
    pub context: &'a str,
    pub installation_id: &'a str,
    pub select: bool,
}

pub fn config_path() -> Result<PathBuf> {
    if let Some(paths) = std::env::var_os("KUBECONFIG")
        && let Some(path) = std::env::split_paths(&paths).find(|path| !path.as_os_str().is_empty())
    {
        return std::path::absolute(path).context("resolve KUBECONFIG");
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .context("home directory is unavailable; set KUBECONFIG")?;
    Ok(PathBuf::from(home).join(".kube/config"))
}

pub fn merge(mut existing: Value, source: &str, profile: &Profile<'_>) -> Result<Value> {
    let mut incoming = validate(source, profile.context)?;
    ensure!(
        !profile.installation_id.is_empty(),
        "installation identity is required"
    );
    ensure!(
        existing["apiVersion"] == "v1" && existing["kind"] == "Config",
        "existing kubeconfig is not a v1 Config"
    );
    let owned = existing["contexts"].as_array().and_then(|contexts| {
        contexts
            .iter()
            .find(|context| context["name"] == profile.context)
    });
    if let Some(context) = owned {
        ensure!(
            context
                .pointer("/context/extensions")
                .and_then(Value::as_array)
                .is_some_and(|extensions| {
                    extensions.iter().any(|extension| {
                        extension["name"] == "lampyr.dev/installation"
                            && extension.pointer("/extension/id").and_then(Value::as_str)
                                == Some(profile.installation_id)
                    })
                }),
            "kubeconfig context {} is not owned by this installation",
            profile.context
        );
    }
    for (field, reference) in [("clusters", "cluster"), ("users", "user")] {
        if existing[field]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item["name"] == profile.context))
        {
            ensure!(
                owned.is_some_and(|context| context["context"][reference] == profile.context),
                "kubeconfig already has an unrelated {field} entry named {}",
                profile.context
            );
        }
    }
    incoming["contexts"][0]["context"]["cluster"] = profile.context.into();
    incoming["contexts"][0]["context"]["user"] = profile.context.into();
    incoming["contexts"][0]["context"]["extensions"] = json!([{
        "name": "lampyr.dev/installation", "extension": { "id": profile.installation_id }
    }]);
    incoming["clusters"][0]["name"] = profile.context.into();
    incoming["users"][0]["name"] = profile.context.into();
    for field in ["clusters", "contexts", "users"] {
        let object = existing
            .as_object_mut()
            .context("existing kubeconfig must be a mapping")?;
        let entries = object.entry(field).or_insert_with(|| json!([]));
        if entries.is_null() {
            *entries = json!([]);
        }
        let entries = entries
            .as_array_mut()
            .with_context(|| format!("existing kubeconfig {field} must be an array"))?;
        entries.retain(|entry| entry["name"] != profile.context);
        entries.push(incoming[field][0].clone());
    }
    if profile.select
        || existing["current-context"]
            .as_str()
            .is_none_or(str::is_empty)
    {
        existing["current-context"] = profile.context.into();
    }
    Ok(existing)
}

pub fn remove(mut existing: Value, context: &str, installation_id: &str) -> Result<Value> {
    ensure!(
        !installation_id.is_empty(),
        "installation identity is required"
    );
    ensure!(
        existing["apiVersion"] == "v1" && existing["kind"] == "Config",
        "existing kubeconfig is not a v1 Config"
    );
    let owned = existing["contexts"]
        .as_array()
        .context("existing kubeconfig contexts must be an array")?
        .iter()
        .find(|entry| entry["name"] == context)
        .cloned();
    let Some(owned) = owned else {
        return Ok(existing);
    };
    ensure!(
        owned
            .pointer("/context/extensions")
            .and_then(Value::as_array)
            .is_some_and(|extensions| {
                extensions.iter().any(|extension| {
                    extension["name"] == "lampyr.dev/installation"
                        && extension.pointer("/extension/id").and_then(Value::as_str)
                            == Some(installation_id)
                })
            }),
        "kubeconfig context {context} is not owned by this installation"
    );
    let cluster = owned
        .pointer("/context/cluster")
        .and_then(Value::as_str)
        .context("owned kubeconfig context has no cluster")?
        .to_owned();
    let user = owned
        .pointer("/context/user")
        .and_then(Value::as_str)
        .context("owned kubeconfig context has no user")?
        .to_owned();
    existing["contexts"]
        .as_array_mut()
        .context("existing kubeconfig contexts must be an array")?
        .retain(|entry| entry["name"] != context);
    for (field, reference, name) in [("clusters", "cluster", cluster), ("users", "user", user)] {
        let referenced = existing["contexts"]
            .as_array()
            .context("existing kubeconfig contexts must be an array")?
            .iter()
            .any(|entry| entry["context"][reference] == name);
        if !referenced {
            existing[field]
                .as_array_mut()
                .with_context(|| format!("existing kubeconfig {field} must be an array"))?
                .retain(|entry| entry["name"] != name);
        }
    }
    if existing["current-context"] == context {
        existing["current-context"] = String::new().into();
    }
    Ok(existing)
}

fn update(path: &Path, action: impl FnOnce(Value) -> Result<Value>) -> Result<()> {
    use std::io::{Read, Write};
    let parent = path.parent().context("kubeconfig path has no parent")?;
    let mut directories = std::fs::DirBuilder::new();
    directories.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        directories.mode(0o700);
    }
    directories.create(parent)?;
    let name = path
        .file_name()
        .context("kubeconfig path has no filename")?
        .to_str()
        .context("kubeconfig filename must be UTF-8")?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let lock = options.open(parent.join(format!(".{name}.lampyr.lock")))?;
    lock.try_lock()
        .context("another Lampyr command is updating this kubeconfig")?;
    let existing = match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                metadata.is_file(),
                "kubeconfig must be a regular file, not a link"
            );
            let mut bytes = Vec::new();
            let mut options = std::fs::OpenOptions::new();
            options.read(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
            }
            let file = options.open(path)?;
            ensure!(
                file.metadata()?.is_file(),
                "kubeconfig must be a regular file"
            );
            file.take(8 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
            ensure!(
                bytes.len() <= 8 * 1024 * 1024,
                "existing kubeconfig exceeds 8 MiB"
            );
            if bytes.iter().all(u8::is_ascii_whitespace) {
                json!({ "apiVersion": "v1", "kind": "Config" })
            } else {
                serde_yaml_ng::from_slice(&bytes).context("decode existing kubeconfig")?
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            json!({ "apiVersion": "v1", "kind": "Config" })
        }
        Err(error) => return Err(error).context("read existing kubeconfig"),
    };
    let updated = action(existing)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(&serde_json::to_vec_pretty(&updated)?)?;
    file.as_file().sync_all()?;
    file.persist(path)
        .context("atomically replace kubeconfig")?;
    Ok(())
}

pub fn persist(path: &Path, source: &str, profile: &Profile<'_>) -> Result<()> {
    update(path, |existing| merge(existing, source, profile))
}

pub fn release(path: &Path, context: &str, installation_id: &str) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => update(path, |existing| remove(existing, context, installation_id)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("read existing kubeconfig"),
    }
}

pub fn validate(source: &str, expected_context: &str) -> Result<Value> {
    ensure!(
        source.len() <= crate::runtime::PAYLOAD_LIMIT,
        "provider kubeconfig exceeds 64 KiB"
    );
    let config: Value = serde_yaml_ng::from_str(source).context("decode provider kubeconfig")?;
    ensure!(
        config["apiVersion"] == "v1" && config["kind"] == "Config",
        "provider returned an unsupported kubeconfig"
    );
    ensure!(
        config["current-context"] == expected_context,
        "provider returned a different Kubernetes context"
    );
    let single = |field: &str| -> Result<&Value> {
        let entries = config[field]
            .as_array()
            .with_context(|| format!("kubeconfig has no {field}"))?;
        ensure!(
            entries.len() == 1,
            "provider kubeconfig must contain exactly one {field} entry"
        );
        Ok(&entries[0])
    };
    let context = single("contexts")?;
    let cluster = single("clusters")?;
    let user = single("users")?;
    ensure!(
        context["name"] == expected_context
            && context["context"]["cluster"] == cluster["name"]
            && context["context"]["user"] == user["name"]
            && cluster["name"]
                .as_str()
                .is_some_and(|name| !name.is_empty())
            && user["name"].as_str().is_some_and(|name| !name.is_empty()),
        "provider kubeconfig references an unexpected cluster or user"
    );
    allowed(
        &cluster["cluster"],
        &[
            "server",
            "certificate-authority-data",
            "tls-server-name",
            "insecure-skip-tls-verify",
        ],
    )?;
    ensure!(
        cluster["cluster"]["insecure-skip-tls-verify"].is_null()
            || cluster["cluster"]["insecure-skip-tls-verify"] == false,
        "provider kubeconfig may not disable TLS verification"
    );
    let server = cluster["cluster"]["server"]
        .as_str()
        .context("kubeconfig has no API server")?;
    let uri: ureq::http::Uri = server.parse().context("invalid Kubernetes API server")?;
    ensure!(
        uri.scheme_str() == Some("https")
            && uri.host().is_some()
            && uri
                .authority()
                .is_some_and(|value| !value.as_str().contains('@')),
        "provider API server must use HTTPS without embedded credentials"
    );
    embedded(&cluster["cluster"], "certificate-authority-data")?;
    allowed(&context["context"], &["cluster", "user", "namespace"])?;
    allowed(
        &user["user"],
        &["client-certificate-data", "client-key-data"],
    )?;
    embedded(&user["user"], "client-certificate-data")?;
    embedded(&user["user"], "client-key-data")?;
    Ok(config)
}

fn allowed(value: &Value, keys: &[&str]) -> Result<()> {
    let object = value
        .as_object()
        .context("kubeconfig entry must be a mapping")?;
    for name in object.keys() {
        ensure!(
            keys.contains(&name.as_str()),
            "provider kubeconfig contains an unauthorized field: {name}"
        );
    }
    Ok(())
}

fn embedded(value: &Value, name: &str) -> Result<()> {
    let bytes = value[name]
        .as_str()
        .with_context(|| format!("kubeconfig must embed {name}"))?;
    ensure!(
        !STANDARD
            .decode(bytes)
            .with_context(|| format!("invalid base64 {name}"))?
            .is_empty(),
        "kubeconfig {name} must not be empty"
    );
    Ok(())
}
