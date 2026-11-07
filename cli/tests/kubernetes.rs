// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use std::collections::BTreeMap;

use lampyr_cli::deploy;
use lampyr_cli::grant;
use lampyr_cli::image::{Metadata, Provenance, Source, Target};
use lampyr_cli::kube;
use lampyr_cli::project::Project;
use lampyr_cli::routing;

#[test]
fn an_unschedulable_grant_pod_reports_the_scheduler_message() {
    use std::time::{Duration, Instant};

    let pod = |uid: &str| {
        serde_json::json!({"metadata": {"uid": uid}, "status": {"conditions": [
            {"type": "Initialized", "status": "True"},
            {
                "type": "PodScheduled", "status": "False", "reason": "Unschedulable",
                "lastTransitionTime": "2026-11-06T00:00:00Z",
                "message": "0/1 nodes are available: 1 Insufficient cpu.",
            },
        ]}})
    };
    let since = chrono::DateTime::parse_from_rfc3339("2026-11-06T00:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let start = Instant::now();
    let message = Some("0/1 nodes are available: 1 Insufficient cpu.".to_owned());

    // A client clock far ahead of the server can't end the grace early:
    // it is timed from the first poll that saw the Pod unschedulable.
    let mut watch = grant::Unschedulable::default();
    let far_ahead = since + chrono::TimeDelta::hours(1);
    assert_eq!(watch.check(&pod("a"), start, far_ahead), None);
    assert_eq!(
        watch.check(&pod("a"), start + Duration::from_secs(10), far_ahead),
        None
    );
    assert_eq!(
        watch.check(&pod("a"), start + grant::UNSCHEDULABLE_GRACE, far_ahead),
        message
    );

    // The server's lastTransitionTime can lengthen it.
    let mut watch = grant::Unschedulable::default();
    let just_now = since + chrono::TimeDelta::seconds(5);
    watch.check(&pod("a"), start, just_now);
    assert_eq!(
        watch.check(&pod("a"), start + grant::UNSCHEDULABLE_GRACE, just_now),
        None
    );

    // A replacement Pod starts its own grace.
    let mut watch = grant::Unschedulable::default();
    watch.check(&pod("a"), start, far_ahead);
    assert_eq!(
        watch.check(&pod("b"), start + grant::UNSCHEDULABLE_GRACE, far_ahead),
        None
    );

    // A scheduled Pod resets the watch.
    let scheduled = serde_json::json!({"status": {"conditions": [
        {"type": "PodScheduled", "status": "True"},
    ]}});
    let mut watch = grant::Unschedulable::default();
    watch.check(&pod("a"), start, far_ahead);
    assert_eq!(
        watch.check(&scheduled, start + Duration::from_secs(5), far_ahead),
        None
    );
    assert_eq!(
        watch.check(&pod("a"), start + grant::UNSCHEDULABLE_GRACE, far_ahead),
        None
    );
}

#[test]
fn traffic_rejects_duplicate_targets_invalid_names_and_incomplete_allocations() {
    let targets = routing::traffic("old=25,current=75", "new").unwrap();
    assert_eq!(targets[1].revision, "new");
    assert_eq!(targets[1].percent, 75);
    for value in [
        "current=50,new=50",
        "old=20,current=70",
        "current=-1,old=101",
        "../other=100",
        "old=hello",
        "old=100,",
    ] {
        assert!(routing::traffic(value, "new").is_err(), "{value}");
    }
    assert_eq!(routing::traffic("", "new").unwrap()[0].percent, 100);
}

#[test]
fn deploy_uses_knative_service_with_snapshot_revision_metadata() {
    let project: Project = serde_json::from_value(serde_json::json!({
        "apiVersion": "lampyr.dev/v1alpha1",
        "kind": "LampyrService",
        "name": "demo",
        "runtime": "nodejs",
        "command": "node app.js",
        "protocols": { "http": true, "events": true },
        "resources": { "cpu": "100m", "memory": "128Mi" }
    }))
    .unwrap();
    let metadata = Metadata {
        schema_version: "lampyr.dev/artifact/v1alpha2".to_owned(),
        revision_id: "0123456789abcdef".to_owned(),
        project,
        target: Target {
            os: "linux".to_owned(),
            arch: "amd64".to_owned(),
            hypervisor: "kvm".to_owned(),
            compatibility_class: "linux-amd64-kvm".to_owned(),
        },
        source: Source {
            digest: format!("sha256:{}", "ab".repeat(32)),
        },
        runtime: BTreeMap::new(),
        artifacts: BTreeMap::new(),
        provenance: Provenance {
            build_type: "test".to_owned(),
            builder: "test".to_owned(),
        },
    };
    let image = format!("registry.example/demo@sha256:{}", "cd".repeat(32));
    let resource = deploy::resources(
        &metadata,
        &format!("sha256:{}", "ef".repeat(32)),
        &image,
        "demo-00001",
        &routing::traffic("", "demo-00001").unwrap(),
        true,
    )
    .unwrap();
    assert_eq!(resource["apiVersion"], "serving.knative.dev/v1");
    assert_eq!(resource["kind"], "Service");
    assert_eq!(
        resource["spec"]["template"]["metadata"]["annotations"]["lampyr.dev/snapshot-artifact"],
        image
    );
    assert_eq!(
        resource["spec"]["template"]["spec"]["containers"][0]["image"],
        image
    );
    assert_eq!(
        resource["spec"]["template"]["spec"]["containerConcurrency"],
        1
    );
    assert_eq!(resource["spec"]["traffic"][0]["revisionName"], "demo-00001");
    assert_eq!(
        resource["metadata"]["annotations"]["lampyr.dev/events-enabled"],
        "true"
    );
    assert_eq!(
        resource["spec"]["template"]["spec"]["containers"][0]["readinessProbe"],
        serde_json::json!({"successThreshold":1,"tcpSocket":{"port":0}})
    );
    // A new Revision gets Knative's default timeout; reapplying an existing
    // one keeps the timeout Knative stored for it.
    assert!(
        resource["spec"]["template"]["spec"]
            .get("timeoutSeconds")
            .is_none()
    );
    let mut reapplied = resource.clone();
    deploy::keep_existing_timeout(&mut reapplied, None);
    assert_eq!(reapplied, resource);
    deploy::keep_existing_timeout(
        &mut reapplied,
        Some(&serde_json::json!({"spec": {"timeoutSeconds": 30}})),
    );
    assert_eq!(reapplied["spec"]["template"]["spec"]["timeoutSeconds"], 30);
    let internal = deploy::resources(
        &metadata,
        &format!("sha256:{}", "ef".repeat(32)),
        &image,
        "demo-00001",
        &routing::traffic("", "demo-00001").unwrap(),
        false,
    )
    .unwrap();
    assert_eq!(
        internal["metadata"]["labels"][kube::VISIBILITY_LABEL],
        "cluster-local"
    );
    assert_eq!(
        internal["spec"], resource["spec"],
        "exposure must not change immutable Revision inputs"
    );

    let mut grpc = metadata;
    grpc.project.protocols.grpc = true;
    let resource = deploy::resources(
        &grpc,
        &format!("sha256:{}", "ef".repeat(32)),
        &image,
        "demo-00001",
        &routing::traffic("", "demo-00001").unwrap(),
        true,
    )
    .unwrap();
    assert_eq!(
        resource["spec"]["template"]["spec"]["containers"][0]["ports"][0],
        serde_json::json!({"name":"h2c","containerPort":8080,"protocol":"TCP"})
    );
    assert_eq!(
        resource["spec"]["template"]["spec"]["containers"][0]["readinessProbe"],
        serde_json::json!({"successThreshold":1,"tcpSocket":{"port":0}})
    );
}

#[test]
fn a_readiness_probe_cannot_report_success_after_its_deadline() {
    let host = lampyr_cli::host::Host {
        verbose: false,
        cancellation: Default::default(),
    };
    let kube = kube::Kube::new(std::path::Path::new("."), Some("test"), &host).unwrap();
    let result = kube.wait("slow probe", std::time::Duration::from_millis(10), || {
        std::thread::sleep(std::time::Duration::from_millis(20));
        Ok(Some(()))
    });
    assert!(result.unwrap_err().to_string().contains("timed out"));
}

#[cfg(unix)]
#[test]
fn kubernetes_reads_allow_credential_refresh_without_extending_readiness_deadlines() {
    use std::process::Command;
    use std::time::{Duration, Instant};

    let host = lampyr_cli::host::Host {
        verbose: false,
        cancellation: Default::default(),
    };
    let kube = kube::Kube::new(std::path::Path::new("."), Some("test"), &host).unwrap();
    assert_eq!(
        kube.capture(Command::new("sh").args(["-c", "sleep 11; printf refreshed"]))
            .unwrap(),
        "refreshed"
    );
    let started = Instant::now();
    let result = kube.wait("bounded read", Duration::from_millis(20), || {
        kube.capture(Command::new("sh").args(["-c", "sleep 5"]))
            .map(Some)
    });
    assert!(result.unwrap_err().to_string().contains("deadline"));
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn existing_resources_must_belong_to_both_lampyr_and_the_selected_service() {
    let mut resource = serde_json::json!({ "metadata": { "labels": {
        "app.kubernetes.io/managed-by": "lampyr", "edge.lampyr.dev/service": "owned"
    } } });
    kube::ensure_owner(&resource, "owned").unwrap();
    assert!(kube::ensure_owner(&resource, "other").is_err());
    resource["metadata"]["labels"]["app.kubernetes.io/managed-by"] = "another-manager".into();
    assert!(kube::ensure_owner(&resource, "owned").is_err());
}

#[cfg(unix)]
#[test]
fn crd_establishment_waits_for_initial_status_but_preserves_deadlines_and_errors() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    if let Ok(scenario) = std::env::var("CRD_SCENARIO") {
        crd_wait_probe(&scenario);
        return;
    }

    let directory = tempfile::tempdir().unwrap();
    let kubectl = directory.path().join("kubectl");
    std::fs::write(
            &kubectl,
            "#!/bin/sh\n\
             read -r count < \"$CRD_COUNT\"\n\
             count=$((count + 1))\n\
             printf '%s\\n' \"$count\" > \"$CRD_COUNT\"\n\
             case \"$CRD_SCENARIO\" in\n\
             denied) printf 'fixture access denied' >&2; exit 71;;\n\
             malformed) printf '{\"status\":{\"conditions\":true}}'; exit 0;;\n\
             timeout) printf '{\"status\":{\"conditions\":null}}'; exit 0;;\n\
             esac\n\
             case \"$count\" in\n\
             1) printf '{\"status\":{\"conditions\":null}}';;\n\
             2) printf '{}';;\n\
             3) printf '{\"status\":{\"conditions\":[{\"type\":\"Established\",\"status\":\"False\"}]}}';;\n\
             *) printf '{\"status\":{\"conditions\":[{\"type\":\"Established\",\"status\":\"True\"}]}}';;\n\
             esac\n",
        )
        .unwrap();
    std::fs::set_permissions(&kubectl, std::fs::Permissions::from_mode(0o700)).unwrap();
    for (scenario, expected_calls) in [
        ("progress", Some(5)),
        ("timeout", None),
        ("denied", Some(1)),
        ("malformed", Some(1)),
    ] {
        let count = directory.path().join(format!("{scenario}.count"));
        std::fs::write(&count, "0\n").unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "crd_establishment_waits_for_initial_status_but_preserves_deadlines_and_errors",
                "--nocapture",
            ])
            .env("PATH", directory.path())
            .env("CRD_SCENARIO", scenario)
            .env("CRD_COUNT", &count)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{scenario}: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if let Some(expected_calls) = expected_calls {
            assert_eq!(
                std::fs::read_to_string(count).unwrap().trim(),
                expected_calls.to_string(),
                "{scenario}"
            );
        }
    }
}

#[cfg(unix)]
fn crd_wait_probe(scenario: &str) {
    use std::time::Duration;

    let host = lampyr_cli::host::Host {
        verbose: false,
        cancellation: Default::default(),
    };
    let kube = kube::Kube::new(std::path::Path::new("."), Some("fixture"), &host).unwrap();
    let timeout = Duration::from_millis(if scenario == "timeout" { 500 } else { 5000 });
    let result = kube.wait_crds(&["first", "second"], timeout);
    match scenario {
        "progress" => result.unwrap(),
        "timeout" => {
            let error = result.unwrap_err().to_string();
            assert!(
                error.contains("timed out") || error.contains("deadline"),
                "{error}"
            );
        }
        "denied" => {
            assert!(format!("{:#}", result.unwrap_err()).contains("fixture access denied"))
        }
        "malformed" => assert!(result.unwrap_err().to_string().contains("must be an array")),
        _ => panic!("unknown CRD fixture scenario"),
    }
}
