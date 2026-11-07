// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use lampyr_cli::connection;
use serde_json::{Value, json};

fn config() -> Value {
    json!({
        "apiVersion": "v1", "kind": "Config", "current-context": "owned",
        "clusters": [{ "name": "owned", "cluster": { "server": "https://127.0.0.1:6443", "certificate-authority-data": "Y2E=" } }],
        "contexts": [{ "name": "owned", "context": { "cluster": "owned", "user": "owned" } }],
        "users": [{ "name": "owned", "user": { "client-certificate-data": "Y2VydA==", "client-key-data": "a2V5" } }]
    })
}

#[test]
fn returned_connections_cannot_execute_commands_or_read_host_credentials() {
    connection::validate(&config().to_string(), "owned").unwrap();
    for (entry, key, value) in [
        ("/users/0/user", "exec", json!({ "command": "/bin/sh" })),
        ("/users/0/user", "auth-provider", json!({ "name": "azure" })),
        (
            "/users/0/user",
            "client-key",
            json!("/home/user/private.key"),
        ),
        ("/users/0/user", "tokenFile", json!("/home/user/token")),
        ("/users/0/user", "token", json!("short-lived")),
        (
            "/clusters/0/cluster",
            "certificate-authority",
            json!("/home/user/ca"),
        ),
        (
            "/clusters/0/cluster",
            "proxy-url",
            json!("http://untrusted"),
        ),
        (
            "/clusters/0/cluster",
            "insecure-skip-tls-verify",
            json!(true),
        ),
    ] {
        let mut changed = config();
        changed.pointer_mut(entry).unwrap()[key] = value;
        assert!(
            connection::validate(&changed.to_string(), "owned").is_err(),
            "{key}"
        );
    }
}

#[test]
fn connections_are_bound_to_the_expected_context_and_embed_their_credentials() {
    assert!(connection::validate(&config().to_string(), "different").is_err());
    for (path, value) in [
        ("/contexts/0/context/user", json!("another")),
        ("/clusters/0/cluster/server", json!("http://127.0.0.1:6443")),
        ("/users/0/user/client-key-data", json!("not-base64")),
        ("/clusters/0/cluster/certificate-authority-data", json!("")),
    ] {
        let mut changed = config();
        *changed.pointer_mut(path).unwrap() = value;
        assert!(
            connection::validate(&changed.to_string(), "owned").is_err(),
            "{path}"
        );
    }
}

#[test]
fn merging_keeps_other_profiles_and_preserves_direct_credentials() {
    let profile = connection::Profile {
        context: "owned",
        installation_id: "installation",
        select: false,
    };
    let previous = json!({
        "apiVersion": "v1", "kind": "Config", "current-context": "unrelated",
        "clusters": [{ "name": "unrelated", "cluster": { "server": "https://other" } }],
        "users": [{ "name": "unrelated", "user": { "exec": { "command": "existing-user-tool" } } }],
        "contexts": [{ "name": "unrelated", "context": { "cluster": "unrelated", "user": "unrelated" } }]
    });
    let merged = connection::merge(previous.clone(), &config().to_string(), &profile).unwrap();
    assert_eq!(merged["current-context"], "unrelated");
    for field in ["clusters", "users", "contexts"] {
        assert_eq!(merged[field][0], previous[field][0]);
    }
    assert_eq!(
        merged["users"][1]["user"]["client-certificate-data"],
        config()["users"][0]["user"]["client-certificate-data"]
    );
    assert_eq!(
        merged["users"][1]["user"]["client-key-data"],
        config()["users"][0]["user"]["client-key-data"]
    );
    assert!(merged["users"][1]["user"]["exec"].is_null());
    connection::merge(merged.clone(), &config().to_string(), &profile).unwrap();
    let mut collision = merged;
    collision["contexts"][1]["context"]["cluster"] = "unrelated".into();
    assert!(connection::merge(collision, &config().to_string(), &profile).is_err());
    assert!(connection::merge(config(), &config().to_string(), &profile).is_err());
}

#[test]
fn releasing_removes_only_the_owned_profile_and_clears_its_selection() {
    let profile = connection::Profile {
        context: "owned",
        installation_id: "installation",
        select: true,
    };
    let unrelated = json!({
        "apiVersion": "v1", "kind": "Config", "current-context": "unrelated",
        "clusters": [{ "name": "unrelated", "cluster": { "server": "https://other" } }],
        "users": [{ "name": "unrelated", "user": { "token": "existing" } }],
        "contexts": [{ "name": "unrelated", "context": { "cluster": "unrelated", "user": "unrelated" } }]
    });
    let selected = connection::merge(unrelated.clone(), &config().to_string(), &profile).unwrap();
    let released = connection::remove(selected, "owned", "installation").unwrap();
    assert_eq!(released["current-context"], "");
    for field in ["clusters", "users", "contexts"] {
        assert_eq!(released[field], unrelated[field]);
    }
    assert!(connection::remove(config(), "owned", "another-installation").is_err());
}

#[test]
fn releasing_an_unselected_profile_preserves_the_current_context() {
    let profile = connection::Profile {
        context: "owned",
        installation_id: "installation",
        select: false,
    };
    let unrelated = json!({
        "apiVersion": "v1", "kind": "Config", "current-context": "unrelated",
        "clusters": [{ "name": "unrelated", "cluster": { "server": "https://other" } }],
        "users": [{ "name": "unrelated", "user": { "token": "existing" } }],
        "contexts": [{ "name": "unrelated", "context": { "cluster": "unrelated", "user": "unrelated" } }]
    });
    let merged = connection::merge(unrelated.clone(), &config().to_string(), &profile).unwrap();
    let released = connection::remove(merged, "owned", "installation").unwrap();
    assert_eq!(released, unrelated);
}

#[test]
fn persisted_profiles_are_private_and_do_not_follow_existing_symlinks() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config");
    let profile = connection::Profile {
        context: "owned",
        installation_id: "installation",
        select: true,
    };
    connection::persist(&path, &config().to_string(), &profile).unwrap();
    let saved: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(saved["current-context"], "owned");
    assert_eq!(
        saved["users"][0]["user"]["client-certificate-data"],
        config()["users"][0]["user"]["client-certificate-data"]
    );
    connection::release(&path, "owned", "installation").unwrap();
    let released: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(released["current-context"], "");
    assert_eq!(released["contexts"], json!([]));
    connection::persist(&path, &config().to_string(), &profile).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(connection::persist(&link, &config().to_string(), &profile).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
}
