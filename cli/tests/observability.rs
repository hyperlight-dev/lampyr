// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use std::process::Command;

#[test]
fn observability_help_exposes_addon_lifecycle() {
    let output = Command::new(env!("CARGO_BIN_EXE_ly"))
        .args(["observability", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);
    for command in ["install", "status", "uninstall"] {
        assert!(help.contains(command), "{help}");
    }
}

#[cfg(unix)]
mod lifecycle {
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use serde_json::{Value, json};

    const ENDPOINT: &str = "http://jaeger.lampyr-observability.svc.cluster.local:4317";
    const OWNER: &str = "lampyr.dev/observability-trace-endpoint";

    fn router(env: Value) -> Value {
        json!({
            "metadata":{"annotations":{OWNER:ENDPOINT}},
            "spec":{"template":{"spec":{"containers":[{"name":"router","env":env}]}}}
        })
    }

    fn fixture(router: Value) -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        std::fs::write(root.join(".lampyr-platform"), "").unwrap();
        std::fs::write(root.join("router.json"), router.to_string()).unwrap();
        let mut disabled = router;
        disabled["spec"]["template"]["spec"]["containers"][0]["env"] = json!([]);
        std::fs::write(root.join("disabled.json"), disabled.to_string()).unwrap();
        std::fs::write(
            root.join("kubectl"),
            r#"#!/bin/sh
set -eu
printf '%s\n' "$*" >> "$OBS_ROOT/commands"
case "$*" in
  *"get deployment fiber-router "*) cat "$OBS_ROOT/router.json" ;;
  *"get deployments,pods,services "*) echo 'deployment.apps/jaeger 1/1' ;;
  *"set env deployment/fiber-router "*)
    if [ "$OBS_FAIL" = disable ]; then echo 'injected disable failure' >&2; exit 1; fi
    cp "$OBS_ROOT/disabled.json" "$OBS_ROOT/router.json"
    ;;
  *"rollout status deployment/fiber-router "*)
    if [ "$OBS_FAIL" = rollout ]; then echo 'injected rollout failure' >&2; exit 1; fi
    ;;
  *"patch deployment fiber-router "*)
    if [ "$OBS_FAIL" = ownership ]; then echo 'injected ownership failure' >&2; exit 1; fi
    ;;
esac
"#,
        )
        .unwrap();
        std::fs::set_permissions(root.join("kubectl"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        directory
    }

    fn run(root: &Path, action: &str, failure: &str) -> std::process::Output {
        let path = std::env::join_paths(
            std::iter::once(root.to_path_buf())
                .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
        )
        .unwrap();
        super::Command::new(env!("CARGO_BIN_EXE_ly"))
            .current_dir(root)
            .env("PATH", path)
            .env("OBS_ROOT", root)
            .env("OBS_FAIL", failure)
            .args(["observability", action, "--context", "test"])
            .output()
            .unwrap()
    }

    #[test]
    fn status_displays_workloads_without_verbose() {
        let directory = fixture(router(json!([])));
        let output = run(directory.path(), "status", "");
        assert!(output.status.success(), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("deployment.apps/jaeger 1/1"),
            "{output:?}"
        );
    }

    #[test]
    fn trace_configuration_targets_only_the_router_container() {
        let mut deployment = router(json!([]));
        deployment["spec"]["template"]["spec"]["containers"]
            .as_array_mut()
            .unwrap()
            .push(json!({
                "name":"sidecar",
                "env":[{"name":"OTEL_EXPORTER_OTLP_ENDPOINT","value":"https://external-sidecar"}]
            }));
        let directory = fixture(deployment);
        for action in ["install", "uninstall"] {
            let output = run(directory.path(), action, "");
            assert!(output.status.success(), "{output:?}");
        }
        let commands = std::fs::read_to_string(directory.path().join("commands")).unwrap();
        let updates: Vec<_> = commands
            .lines()
            .filter(|command| command.contains(" set env deployment/fiber-router "))
            .collect();
        assert_eq!(updates.len(), 2, "{commands}");
        for command in updates {
            assert!(
                command
                    .split_whitespace()
                    .any(|arg| arg == "--containers=router"),
                "trace update must not change the sidecar: {command}"
            );
        }
    }

    #[test]
    fn install_reports_a_failed_trace_rollback() {
        let directory = fixture(router(json!([])));
        let output = run(directory.path(), "install", "rollout");
        assert!(!output.status.success(), "{output:?}");
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("rollback"), "{error}");
        assert!(error.contains("injected rollout failure"), "{error}");
    }

    #[test]
    fn failed_reinstall_restores_the_original_owned_trace_settings() {
        for env in [
            json!([
                {"name":"OTEL_EXPORTER_OTLP_ENDPOINT","value":ENDPOINT},
                {"name":"OTEL_EXPORTER_OTLP_INSECURE","value":"true"}
            ]),
            json!([{"name":"OTEL_EXPORTER_OTLP_ENDPOINT","value":ENDPOINT}]),
            json!([{"name":"OTEL_EXPORTER_OTLP_INSECURE","value":"true"}]),
            json!([]),
        ] {
            for failure in ["rollout", "ownership"] {
                let directory = fixture(router(env.clone()));
                let output = run(directory.path(), "install", failure);
                assert!(!output.status.success(), "{output:?}");
                assert!(
                    String::from_utf8_lossy(&output.stderr)
                        .contains(&format!("injected {failure} failure")),
                    "{output:?}"
                );
                let commands = std::fs::read_to_string(directory.path().join("commands")).unwrap();
                let restored = commands
                    .lines()
                    .rfind(|command| command.contains(" set env deployment/fiber-router "))
                    .unwrap();
                for name in ["OTEL_EXPORTER_OTLP_ENDPOINT", "OTEL_EXPORTER_OTLP_INSECURE"] {
                    let original = env
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|entry| entry["name"] == name);
                    let expected = match original {
                        Some(entry) => format!("{name}={}", entry["value"].as_str().unwrap()),
                        None => format!("{name}-"),
                    };
                    assert!(
                        restored.split_whitespace().any(|arg| arg == expected),
                        "failed reinstall must restore {expected}: {commands}"
                    );
                }
                assert!(
                    !commands.contains(&format!("\"{OWNER}\":null")),
                    "{commands}"
                );
            }
        }
    }

    #[test]
    fn uninstall_keeps_backend_and_ownership_until_router_rollout_succeeds() {
        for failure in ["disable", "rollout"] {
            let directory = fixture(router(json!([
                {"name":"OTEL_EXPORTER_OTLP_ENDPOINT","value":ENDPOINT},
                {"name":"OTEL_EXPORTER_OTLP_INSECURE","value":"true"}
            ])));
            let root = directory.path();
            let output = run(root, "uninstall", failure);
            assert!(!output.status.success(), "{output:?}");
            assert!(
                String::from_utf8_lossy(&output.stderr)
                    .contains(&format!("injected {failure} failure")),
                "{output:?}"
            );
            let commands = std::fs::read_to_string(root.join("commands")).unwrap();
            assert!(!commands.contains("delete --filename"), "{commands}");
            assert!(
                !commands.contains(&format!("\"{OWNER}\":null")),
                "{commands}"
            );

            let output = run(root, "uninstall", "");
            assert!(output.status.success(), "{output:?}");
            let commands = std::fs::read_to_string(root.join("commands")).unwrap();
            let rollout = commands
                .rfind("rollout status deployment/fiber-router")
                .unwrap();
            let release = commands.find(&format!("\"{OWNER}\":null")).unwrap();
            let delete = commands.find("delete --filename").unwrap();
            assert!(rollout < release && release < delete, "{commands}");
        }
    }

    #[test]
    fn uninstall_does_not_remove_external_trace_settings_with_a_stale_owner() {
        for endpoint in [
            json!({"name":"OTEL_EXPORTER_OTLP_ENDPOINT","value":"https://external"}),
            json!({"name":"OTEL_EXPORTER_OTLP_ENDPOINT","valueFrom":{"secretKeyRef":{"name":"otel","key":"endpoint"}}}),
        ] {
            let directory = fixture(router(json!([endpoint])));
            let root = directory.path();
            let output = run(root, "uninstall", "");
            assert!(!output.status.success(), "{output:?}");
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("configured outside"),
                "{output:?}"
            );
            let commands = std::fs::read_to_string(root.join("commands")).unwrap();
            assert!(!commands.contains("set env"), "{commands}");
            assert!(!commands.contains("delete --filename"), "{commands}");
        }
    }
}
