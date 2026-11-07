// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use std::path::Path;
use std::time::Duration;

use anyhow::{Result, ensure};
use serde_json::json;

use crate::bootstrap::{apply_url, delete_manifest, rollout, run};
use crate::kube::Kube;

const RELEASE: &str = "https://github.com/knative/eventing/releases/download/knative-v1.23.0";
const NAMESPACE: &str = "knative-eventing";
const MANIFESTS: [&str; 4] = [
    "eventing-crds.yaml",
    "eventing-core.yaml",
    "in-memory-channel.yaml",
    "mt-channel-broker.yaml",
];
const DEPLOYMENTS: [&str; 8] = [
    "eventing-controller",
    "eventing-webhook",
    "job-sink",
    "imc-controller",
    "imc-dispatcher",
    "mt-broker-controller",
    "mt-broker-filter",
    "mt-broker-ingress",
];

pub fn install_core(kube: &Kube<'_>) -> Result<()> {
    let installed = kube.installation()?;
    if installed
        .data
        .get("eventing")
        .is_none_or(|value| value != "true")
    {
        ensure!(
            kube.optional("default", "namespace", NAMESPACE)?.is_none(),
            "Eventing already exists and is not owned by this installation"
        );
        kube.patch(
            "kube-system",
            "configmap",
            "lampyr-install",
            &json!({"data":{"eventing":"true"}}),
        )?;
    }
    kube.host.step("Connecting events (Knative Eventing)", || {
        apply_url(kube, &format!("{RELEASE}/{}", MANIFESTS[0]))?;
        kube.wait_crds(
            &[
                "brokers.eventing.knative.dev",
                "triggers.eventing.knative.dev",
            ],
            Duration::from_secs(180),
        )?;
        apply_url(kube, &format!("{RELEASE}/{}", MANIFESTS[1]))?;
        for name in &DEPLOYMENTS[..3] {
            rollout(kube, NAMESPACE, "deployment", name, "180s")?;
        }
        rollout(kube, NAMESPACE, "statefulset", "request-reply", "180s")?;
        apply_url(kube, &format!("{RELEASE}/{}", MANIFESTS[2]))?;
        kube.wait_crds(
            &["inmemorychannels.messaging.knative.dev"],
            Duration::from_secs(180),
        )?;
        for name in &DEPLOYMENTS[3..5] {
            rollout(kube, NAMESPACE, "deployment", name, "180s")?;
        }
        apply_url(kube, &format!("{RELEASE}/{}", MANIFESTS[3]))?;
        for name in &DEPLOYMENTS[5..] {
            rollout(kube, NAMESPACE, "deployment", name, "180s")?;
        }
        crate::bootstrap::single_node_pdbs(kube)
    })
}

pub fn health(kube: &Kube<'_>) -> Result<()> {
    for name in DEPLOYMENTS {
        rollout(kube, NAMESPACE, "deployment", name, "10s")?;
    }
    rollout(kube, NAMESPACE, "statefulset", "request-reply", "10s")
}

pub fn uninstall(kube: &Kube<'_>, _root: &Path) -> Result<()> {
    // Keep core controllers alive while their resources finish finalizing.
    for resource in [
        "triggers.eventing.knative.dev",
        "brokers.eventing.knative.dev",
        "apiserversources.sources.knative.dev",
        "containersources.sources.knative.dev",
        "integrationsources.sources.knative.dev",
        "pingsources.sources.knative.dev",
        "sinkbindings.sources.knative.dev",
        "parallels.flows.knative.dev",
        "sequences.flows.knative.dev",
        "requestreplies.eventing.knative.dev",
        "eventpolicies.eventing.knative.dev",
        "eventtransforms.eventing.knative.dev",
        "integrationsinks.sinks.knative.dev",
        "jobsinks.sinks.knative.dev",
        "subscriptions.messaging.knative.dev",
        "channels.messaging.knative.dev",
        "inmemorychannels.messaging.knative.dev",
        "eventtypes.eventing.knative.dev",
    ] {
        if kube.optional("default", "crd", resource)?.is_some() {
            run(
                kube,
                None,
                &[
                    "delete",
                    resource,
                    "--all",
                    "--all-namespaces",
                    "--ignore-not-found=true",
                    "--wait=true",
                    "--timeout=120s",
                ],
            )?;
        }
    }
    for manifest in MANIFESTS.into_iter().rev() {
        delete_manifest(kube, &format!("{RELEASE}/{manifest}"))?;
    }
    run(
        kube,
        None,
        &[
            "delete",
            "namespace",
            NAMESPACE,
            "--ignore-not-found=true",
            "--wait=true",
            "--timeout=300s",
        ],
    )
}
