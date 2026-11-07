// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

//! Reads the CapacityGrant Lampyr's serving controller writes for each
//! Revision, and the grant Pods fiberd-k8s-controller runs for it. The
//! controller is the source of truth for both: it reports a grant's state
//! in Ready and Progressing conditions, runs its Pods from a Deployment
//! named `fiberd-<grant>`, and labels them with the grant's name and UID.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;

use crate::kube::Kube;

pub const RESOURCE: &str = "capacitygrants.fiberd.io";
/// The controller labels every object it creates for a grant with these,
/// and selects the grant's Pods by both, so a grant recreated under the
/// same name never selects the Pods of the one before it.
const LABEL_GRANT: &str = "fiberd.io/grant";
const LABEL_GRANT_UID: &str = "fiberd.io/grant-uid";
/// The controller's hash of the grant, on its Deployment's Pod template and
/// so on every Pod built from it.
const GRANT_HASH: &str = "fiberd.io~1grant-hash";
/// The Ready reasons for which the controller applies nothing until the
/// spec changes, so waiting would never end.
const REFUSED: &[&str] = &[
    "InvalidSpec",
    "Forbidden",
    "DeploymentConflict",
    "ServiceConflict",
];
/// How long `ly scale` waits for the resized grant. The grant Pod holds the
/// node's hypervisor device, so the controller replaces it with Recreate:
/// the old Pod drains before the new one pulls its helper and template.
const SCALE_TIMEOUT: Duration = Duration::from_secs(240);

#[derive(Deserialize)]
pub struct Grant {
    pub metadata: Identity,
    pub spec: Spec,
    #[serde(default)]
    pub status: Status,
}

#[derive(Deserialize)]
pub struct Identity {
    pub name: String,
    pub namespace: String,
    pub uid: String,
    #[serde(default)]
    pub generation: i64,
}

#[derive(Deserialize)]
pub struct Spec {
    pub fibers: Fibers,
}

#[derive(Deserialize)]
pub struct Fibers {
    pub max: i64,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Status {
    pub observed_generation: i64,
    pub conditions: Vec<Condition>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
pub struct Condition {
    #[serde(rename = "type")]
    pub kind: String,
    pub status: String,
    pub reason: String,
    pub message: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Readiness {
    Ready,
    Pending(String),
    Failed(String),
}

impl Grant {
    /// The label selector of this grant's Pods.
    pub fn pod_selector(&self) -> String {
        format!(
            "{LABEL_GRANT}={},{LABEL_GRANT_UID}={}",
            self.metadata.name, self.metadata.uid
        )
    }

    /// The Deployment the controller runs this grant's Pods from.
    pub fn deployment(&self) -> String {
        format!("fiberd-{}", self.metadata.name)
    }

    fn condition(&self, kind: &str) -> Option<&Condition> {
        self.status
            .conditions
            .iter()
            .find(|condition| condition.kind == kind)
    }

    /// Reads the controller's conditions for the spec the grant holds, as
    /// Lampyr's serving controller does. Ready alone is not enough: while
    /// the controller replaces a Pod, the old one can keep the grant Ready,
    /// so the rollout must have finished too. A rollout the Deployment gave
    /// up on, or a spec the controller refuses, is a failure, not a wait.
    pub fn readiness(&self) -> Readiness {
        self.readiness_with(true)
    }

    /// [`Grant::readiness`], reading `Progressing=False` as a failure only
    /// if `trust_progressing`, and otherwise as a rollout still under way.
    fn readiness_with(&self, trust_progressing: bool) -> Readiness {
        if self.status.observed_generation != self.metadata.generation {
            return Readiness::Pending("CapacityGrant status is not current".to_owned());
        }
        let describe = |condition: Option<&Condition>| match condition {
            Some(condition) if !condition.message.is_empty() => condition.message.clone(),
            Some(condition) if !condition.reason.is_empty() => condition.reason.clone(),
            _ => "no condition reported yet".to_owned(),
        };
        let progressing = self.condition("Progressing");
        if trust_progressing && progressing.is_some_and(|condition| condition.status == "False") {
            return Readiness::Failed(format!(
                "CapacityGrant rollout failed: {}",
                describe(progressing)
            ));
        }
        let ready = self.condition("Ready");
        if ready.map(|condition| condition.status.as_str()) != Some("True") {
            if ready.is_some_and(|condition| REFUSED.contains(&condition.reason.as_str())) {
                return Readiness::Failed(format!("CapacityGrant is refused: {}", describe(ready)));
            }
            return Readiness::Pending(format!("CapacityGrant is not ready: {}", describe(ready)));
        }
        if progressing.map(|condition| condition.reason.as_str()) != Some("NewReplicaSetAvailable")
        {
            return Readiness::Pending(format!(
                "CapacityGrant is rolling out: {}",
                describe(progressing)
            ));
        }
        Readiness::Ready
    }
}

/// The controller's grant hash on `deployment`'s Pod template.
pub fn template_hash(deployment: &Value) -> Option<&str> {
    deployment
        .pointer(&format!("/spec/template/metadata/annotations/{GRANT_HASH}"))
        .and_then(Value::as_str)
}

/// Returns the hash of the Pod template the controller built from `grant`
/// once the grant holds `max` and the controller has observed it.
///
/// The pass that first observes a new spec computes its status from the
/// Deployment as it was before that pass applied the spec, so the grant's
/// conditions can still describe the old Pod. The controller patches that
/// status only after the apply, though, and `deployment` is read after
/// `grant`, so it already holds the new template: its hash and rollout
/// describe the new limit.
///
/// So the grant's Progressing=False, which may be the old Deployment's
/// missed deadline, fails the resize only once `deployment` has observed
/// its current template and reports the same.
pub fn resized_template<'a>(
    grant: &Grant,
    deployment: Option<&'a Value>,
    max: i64,
) -> Result<Option<&'a str>> {
    if grant.spec.fibers.max != max || grant.status.observed_generation != grant.metadata.generation
    {
        return Ok(None);
    }
    if let Readiness::Failed(reason) = grant.readiness_with(deployment.is_some_and(gave_up)) {
        bail!("{reason}");
    }
    Ok(deployment.and_then(template_hash))
}

/// Whether `deployment` has given up rolling out its current template.
fn gave_up(deployment: &Value) -> bool {
    let int = |pointer: &str| deployment.pointer(pointer).and_then(Value::as_i64);
    int("/metadata/generation").is_some()
        && int("/metadata/generation") == int("/status/observedGeneration")
        && deployment
            .pointer("/status/conditions")
            .and_then(Value::as_array)
            .is_some_and(|conditions| {
                conditions.iter().any(|condition| {
                    condition["type"] == "Progressing" && condition["status"] == "False"
                })
            })
}

/// Reports whether `deployment` has rolled out its current template to
/// every replica and kept no Pod of an older one, as `kubectl rollout
/// status` reads it.
pub fn rolled_out(deployment: &Value) -> bool {
    let int = |pointer: &str| deployment.pointer(pointer).and_then(Value::as_i64);
    let replicas = int("/spec/replicas").unwrap_or(1);
    int("/metadata/generation") == int("/status/observedGeneration")
        && int("/status/updatedReplicas") == Some(replicas)
        && int("/status/replicas") == Some(replicas)
        && int("/status/availableReplicas").unwrap_or(0) >= replicas
}

/// The grant Pod built from the template with `hash`, if one exists and
/// isn't being deleted. Until then only Pods of an older limit exist, and
/// whether they can be scheduled says nothing about the new one.
pub fn current_home<'a>(pods: &'a [Value], hash: &str) -> Option<&'a Value> {
    pods.iter().find(|pod| {
        pod.pointer("/metadata/deletionTimestamp").is_none()
            && pod
                .pointer(&format!("/metadata/annotations/{GRANT_HASH}"))
                .and_then(Value::as_str)
                == Some(hash)
    })
}

/// A grant Pod whose fiberd container runs, so its helper log and audit
/// spool can be read.
#[derive(Debug, PartialEq, Eq)]
pub struct RunningPod<'a> {
    pub name: &'a str,
    /// fiberd's grant UID in this Pod. The controller mints each Pod a
    /// grant of its own under the Pod's UID, not the CapacityGrant's, so
    /// fiberd stamps its audit records and names its run directory with it.
    pub uid: &'a str,
}

/// The grant Pods of `pods` whose fiberd container runs and has started. A
/// Pod's Running phase isn't enough: it stays Running while the container
/// crash-loops.
pub fn running_pods(pods: &[Value]) -> Vec<RunningPod<'_>> {
    pods.iter()
        .filter(|pod| pod.pointer("/status/phase").and_then(Value::as_str) == Some("Running"))
        .filter(|pod| {
            pod.pointer("/status/containerStatuses")
                .and_then(Value::as_array)
                .is_some_and(|statuses| {
                    statuses.iter().any(|status| {
                        status["name"] == "fiberd"
                            && status["state"]["running"].is_object()
                            && status["started"] != false
                    })
                })
        })
        .filter_map(|pod| {
            Some(RunningPod {
                name: pod.pointer("/metadata/name").and_then(Value::as_str)?,
                uid: pod.pointer("/metadata/uid").and_then(Value::as_str)?,
            })
        })
        .collect()
}

/// Waits until the controller reports grant `name` ready at `max` Fibers
/// with its resized Pod rolled out. It fails as soon as the controller
/// refuses the spec or gives up on the rollout, or the resized Pod has been
/// unschedulable for [`UNSCHEDULABLE_GRACE`].
pub fn wait(kube: &Kube<'_>, name: &str, max: i32) -> Result<Grant> {
    let max = i64::from(max);
    let mut unschedulable = Unschedulable::default();
    kube.wait("CapacityGrant resize", SCALE_TIMEOUT, || {
        let grant: Grant = kube.get("default", RESOURCE, name)?;
        let namespace = &grant.metadata.namespace;
        let deployment = kube.optional(namespace, "deployment", &grant.deployment())?;
        let Some(hash) = resized_template(&grant, deployment.as_ref(), max)? else {
            unschedulable = Unschedulable::default();
            return Ok(None);
        };
        let pods: Vec<Value> = kube
            .list(namespace, "pods", &grant.pod_selector())
            .context("list grant Pods")?;
        if let Some(pod) = current_home(&pods, hash) {
            if let Some(reason) = unschedulable.check(pod, std::time::Instant::now(), Utc::now()) {
                let pod = pod
                    .pointer("/metadata/name")
                    .and_then(Value::as_str)
                    .unwrap_or("of the grant");
                bail!("grant Pod {pod} can't be scheduled: {reason}");
            }
        } else {
            unschedulable = Unschedulable::default();
        }
        let done =
            grant.readiness() == Readiness::Ready && deployment.as_ref().is_some_and(rolled_out);
        Ok(done.then_some(grant))
    })
}

/// How long a grant Pod may stay unschedulable before `ly scale` gives up.
/// A cluster autoscaler reports Unschedulable before it adds a node.
pub const UNSCHEDULABLE_GRACE: Duration = Duration::from_secs(30);

/// Watches a grant Pod across polls and reports the scheduler's message
/// once the Pod has been unschedulable for [`UNSCHEDULABLE_GRACE`], for
/// example because a resized grant asks for more CPU or memory than a node
/// has. Waiting longer won't help, so `ly scale` fails instead of timing
/// out.
///
/// The grace is timed locally from the first poll that saw this Pod
/// unschedulable. The Pod's lastTransitionTime, measured against the
/// client's wall clock `wall_now` since kubectl reports no server time, can
/// only lengthen it, so a client clock that runs ahead of the API server
/// can't end it early.
#[derive(Default)]
pub struct Unschedulable {
    seen: Option<(String, std::time::Instant)>,
}

impl Unschedulable {
    pub fn check(
        &mut self,
        pod: &Value,
        now: std::time::Instant,
        wall_now: DateTime<Utc>,
    ) -> Option<String> {
        let Some(condition) = pod
            .pointer("/status/conditions")
            .and_then(Value::as_array)
            .and_then(|conditions| {
                conditions.iter().find(|condition| {
                    condition["type"] == "PodScheduled"
                        && condition["status"] == "False"
                        && condition["reason"] == "Unschedulable"
                })
            })
        else {
            self.seen = None;
            return None;
        };
        let uid = pod
            .pointer("/metadata/uid")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let first = match &self.seen {
            Some((seen, first)) if *seen == uid => *first,
            _ => {
                self.seen = Some((uid, now));
                now
            }
        };
        let mut elapsed = now.saturating_duration_since(first);
        if let Some(since) = condition["lastTransitionTime"]
            .as_str()
            .and_then(|time| DateTime::parse_from_rfc3339(time).ok())
        {
            let reported = (wall_now - since.with_timezone(&Utc))
                .to_std()
                .unwrap_or_default();
            elapsed = elapsed.min(reported);
        }
        (elapsed >= UNSCHEDULABLE_GRACE).then(|| {
            condition["message"]
                .as_str()
                .unwrap_or("no node fits it")
                .to_owned()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const HASH: &str = "0123456789abcdef";
    const OLD_HASH: &str = "fedcba9876543210";

    fn grant(max: i64, generation: i64, observed: i64, conditions: Value) -> Grant {
        serde_json::from_value(json!({
            "metadata": {"name": "demo-00001", "namespace": "default", "uid": "uid-1",
                "generation": generation},
            "spec": {"fibers": {"max": max}},
            "status": {"observedGeneration": observed, "conditions": conditions},
        }))
        .unwrap()
    }

    fn conditions(ready: (&str, &str), progressing: (&str, &str)) -> Value {
        json!([
            {"type": "Ready", "status": ready.0, "reason": ready.1, "message": ""},
            {"type": "Progressing", "status": progressing.0, "reason": progressing.1,
                "message": format!("{} message", progressing.1)},
        ])
    }

    fn ready() -> Value {
        conditions(("True", "Available"), ("True", "NewReplicaSetAvailable"))
    }

    fn deployment(hash: &str, generation: i64, observed: i64, updated: i64) -> Value {
        json!({
            "metadata": {"generation": generation},
            "spec": {"replicas": 1, "template": {"metadata": {"annotations": {
                "fiberd.io/grant-hash": hash}}}},
            "status": {"observedGeneration": observed, "replicas": 1,
                "updatedReplicas": updated, "availableReplicas": updated},
        })
    }

    #[test]
    fn readiness_follows_the_controller_conditions_for_the_current_spec() {
        assert_eq!(grant(100, 2, 2, ready()).readiness(), Readiness::Ready);
        assert!(matches!(
            grant(100, 3, 2, ready()).readiness(),
            Readiness::Pending(message) if message.contains("not current")
        ));
        // An old Pod keeps the grant Ready while its replacement rolls out.
        assert!(matches!(
            grant(100, 2, 2, conditions(("True", "Available"), ("True", "ReplicaSetUpdated")))
                .readiness(),
            Readiness::Pending(message) if message.contains("rolling out: ReplicaSetUpdated message")
        ));
        assert!(matches!(
            grant(100, 2, 2, conditions(("False", "Unavailable"), ("True", "ReplicaSetUpdated")))
                .readiness(),
            Readiness::Pending(message) if message.contains("not ready: Unavailable")
        ));
        assert!(matches!(
            grant(100, 2, 2, json!([])).readiness(),
            Readiness::Pending(message) if message.contains("no condition reported yet")
        ));
        for reason in REFUSED {
            assert!(matches!(
                grant(100, 2, 2, conditions(("False", reason), ("Unknown", reason))).readiness(),
                Readiness::Failed(message) if message.contains("refused")
            ));
        }
        assert!(matches!(
            grant(100, 2, 2, conditions(("True", "Available"), ("False", "ProgressDeadlineExceeded")))
                .readiness(),
            Readiness::Failed(message)
                if message == "CapacityGrant rollout failed: ProgressDeadlineExceeded message"
        ));
    }

    #[test]
    fn only_the_template_built_from_the_new_limit_counts() {
        let applied = deployment(HASH, 2, 2, 1);
        // The serving controller hasn't written the new limit yet.
        let stale = grant(100, 1, 1, ready());
        assert_eq!(resized_template(&stale, Some(&applied), 200).unwrap(), None);
        // The controller hasn't observed the new spec.
        let unobserved = grant(200, 2, 1, ready());
        assert_eq!(
            resized_template(&unobserved, Some(&applied), 200).unwrap(),
            None
        );
        let observed = grant(200, 2, 2, ready());
        assert_eq!(resized_template(&observed, None, 200).unwrap(), None);
        assert_eq!(
            resized_template(&observed, Some(&applied), 200).unwrap(),
            Some(HASH)
        );
        // A scale to the limit the grant already holds changes nothing.
        let unchanged = grant(100, 1, 1, ready());
        let old = deployment(OLD_HASH, 1, 1, 1);
        assert_eq!(
            resized_template(&unchanged, Some(&old), 100).unwrap(),
            Some(OLD_HASH)
        );
        let refused = grant(
            200,
            2,
            2,
            conditions(("False", "Forbidden"), ("Unknown", "Forbidden")),
        );
        assert!(
            resized_template(&refused, Some(&applied), 200)
                .unwrap_err()
                .to_string()
                .contains("refused")
        );
    }

    #[test]
    fn a_missed_deadline_fails_a_resize_only_once_the_new_template_reports_it() {
        let failed = grant(
            200,
            2,
            2,
            conditions(("True", "Available"), ("False", "ProgressDeadlineExceeded")),
        );
        let progressing = |status: &str| {
            json!([{"type": "Progressing", "status": status,
                "reason": "ProgressDeadlineExceeded"}])
        };
        // The controller's first pass after the resize reports the old
        // Deployment's missed deadline, which its new template resets.
        let mut applied = deployment(HASH, 3, 3, 0);
        applied["status"]["conditions"] = progressing("True");
        assert_eq!(
            resized_template(&failed, Some(&applied), 200).unwrap(),
            Some(HASH)
        );
        // Nor does a Deployment that hasn't observed its new template yet
        // speak for it.
        let mut unobserved = deployment(HASH, 3, 2, 0);
        unobserved["status"]["conditions"] = progressing("False");
        assert_eq!(
            resized_template(&failed, Some(&unobserved), 200).unwrap(),
            Some(HASH)
        );
        assert_eq!(resized_template(&failed, None, 200).unwrap(), None);
        // The new template's own deadline is a failure.
        let mut stuck = deployment(HASH, 3, 3, 0);
        stuck["status"]["conditions"] = progressing("False");
        assert!(
            resized_template(&failed, Some(&stuck), 200)
                .unwrap_err()
                .to_string()
                .contains("rollout failed")
        );
        // A refused spec fails whatever the Deployment says.
        let refused = grant(
            200,
            2,
            2,
            conditions(("False", "Forbidden"), ("False", "Forbidden")),
        );
        assert!(resized_template(&refused, Some(&applied), 200).is_err());
        // The grant's status alone still reports the deadline.
        assert!(matches!(failed.readiness(), Readiness::Failed(_)));
    }

    #[test]
    fn a_rollout_finishes_when_every_replica_runs_the_current_template() {
        assert!(rolled_out(&deployment(HASH, 2, 2, 1)));
        assert!(!rolled_out(&deployment(HASH, 2, 1, 1)));
        assert!(!rolled_out(&deployment(HASH, 2, 2, 0)));
        // Recreate has stopped the old Pod but the new one isn't available.
        let mut starting = deployment(HASH, 2, 2, 1);
        starting["status"]["availableReplicas"] = 0.into();
        assert!(!rolled_out(&starting));
        // A surge Pod of the old template is still counted.
        let mut surge = deployment(HASH, 2, 2, 1);
        surge["status"]["replicas"] = 2.into();
        assert!(!rolled_out(&surge));
    }

    #[test]
    fn the_current_home_is_the_live_pod_of_the_current_template() {
        let pod = |name: &str, hash: &str, deleting: bool| {
            let mut pod = json!({"metadata": {"name": name, "annotations": {
                "fiberd.io/grant-hash": hash}}});
            if deleting {
                pod["metadata"]["deletionTimestamp"] = "2026-11-06T00:00:00Z".into();
            }
            pod
        };
        let pods = [
            pod("old", OLD_HASH, false),
            pod("leaving", HASH, true),
            pod("new", HASH, false),
        ];
        assert_eq!(
            current_home(&pods, HASH).and_then(|pod| pod.pointer("/metadata/name")),
            Some(&json!("new"))
        );
        assert!(current_home(&pods[..2], HASH).is_none());
    }

    #[test]
    fn grant_pods_are_selected_by_the_controller_labels() {
        let grant = grant(100, 1, 1, ready());
        assert_eq!(
            grant.pod_selector(),
            "fiberd.io/grant=demo-00001,fiberd.io/grant-uid=uid-1"
        );
        assert_eq!(grant.deployment(), "fiberd-demo-00001");
        let pod = |name: &str, phase: &str, state: Value, started: bool| {
            json!({"metadata": {"name": name, "uid": format!("pod-{name}")},
            "status": {"phase": phase, "containerStatuses": [
                {"name": "helper", "state": {"running": {}}, "started": true},
                {"name": "fiberd", "state": state, "started": started},
            ]}})
        };
        let running = json!({"running": {"startedAt": "2026-11-06T00:00:00Z"}});
        let crashing = json!({"waiting": {"reason": "CrashLoopBackOff"}});
        let pods = [
            pod("a", "Pending", running.clone(), true),
            pod("b", "Running", running.clone(), true),
            // A Running Pod whose fiberd crash-loops or hasn't started.
            pod("c", "Running", crashing, false),
            pod("d", "Running", running, false),
            json!({"metadata": {"name": "e", "uid": "pod-e"}, "status": {"phase": "Running"}}),
        ];
        assert_eq!(
            running_pods(&pods),
            [RunningPod {
                name: "b",
                uid: "pod-b"
            }]
        );
    }
}
