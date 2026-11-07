// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use url::Url;

use crate::grant::{self, Grant, Readiness};
use crate::host::Host;
use crate::kube::{self, Installation, Kube};
use crate::project::{Project, validate_name};
use crate::routing::Traffic;

const MAX_SCALE_ANNOTATION: &str = "autoscaling.knative.dev/max-scale";
/// The most Fibers one home may run: 4096 × 16 MiB is Lampyr's serving
/// controller's 64 GiB cap on a home's memory ceiling.
const MAX_FIBERS: i32 = 4096;
/// The grant Pod's fiberd -state directory, which holds its audit spool and,
/// under run/, a directory per grant UID.
const STATE_DIR: &str = "/var/lib/fiberd";
/// The trailing lines of a helper log or audit spool `ly logs` and `ly
/// audit` filter.
const LOG_LINES: usize = 5000;
/// The trailing 7 MiB of either that a grant Pod sends. Both grow with
/// every record and fiberd rotates neither, and a Pod whose output passes
/// the 8 MiB `ly` takes from kubectl is skipped, as 5000 long records alone
/// could make it. A record the cut splits fails to parse and is dropped.
const LOG_BYTES: &str = "--bytes=7340032";

pub struct Status {
    pub service: String,
    pub application_model: String,
    pub revision: String,
    pub url: String,
    pub traffic: Vec<Traffic>,
    pub capacity: Vec<Capacity>,
}

/// What fiberd-k8s-controller reports about one active Revision's grant.
/// The grant Pod serves only mutual TLS to its one caller, fiber-router, so
/// `ly` can't read the Pod's running Fibers or epoch, only the grant.
pub struct Capacity {
    pub revision: String,
    pub grant: Option<(i64, Readiness)>,
}

impl Capacity {
    pub fn describe(&self) -> String {
        let revision = &self.revision;
        match &self.grant {
            None => format!("capacity {revision} missing: no CapacityGrant yet"),
            Some((max, Readiness::Ready)) => {
                format!("capacity {revision} ready, max {max} fibers")
            }
            Some((max, Readiness::Pending(reason))) => {
                format!("capacity {revision} pending, max {max} fibers: {reason}")
            }
            Some((max, Readiness::Failed(reason))) => {
                format!("capacity {revision} failed, max {max} fibers: {reason}")
            }
        }
    }
}

pub fn status(directory: &Path, context: Option<&str>, host: &Host) -> Result<Status> {
    let project = Project::load(directory)?;
    validate_name(&project.name)?;
    let kube = Kube::new(directory, context, host)?;
    let service: Value = kube.get("default", "service.serving.knative.dev", &project.name)?;
    let latest_ready = service
        .pointer("/status/latestReadyRevisionName")
        .and_then(Value::as_str)
        .context("Knative Service has no ready Revision")?
        .to_owned();
    let installed = kube.installation()?;
    let traffic = service
        .pointer("/status/traffic")
        .and_then(Value::as_array)
        .context("Knative Service has no ready traffic")?
        .iter()
        .map(|target| {
            Ok(Traffic {
                revision: target
                    .get("revisionName")
                    .and_then(Value::as_str)
                    .context("Knative traffic target has no Revision")?
                    .to_owned(),
                percent: target
                    .get("percent")
                    .and_then(Value::as_i64)
                    .context("Knative traffic target has no percentage")?
                    .try_into()
                    .context("Knative traffic percentage is invalid")?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let revisions = active_revisions(&service)?;
    let revision = if revisions.len() == 1 {
        revisions
            .first()
            .context("Knative Service has no active Revision")?
            .to_owned()
    } else {
        latest_ready
    };
    let capacity = revisions
        .into_iter()
        .map(|revision| {
            let grant = kube
                .optional("default", grant::RESOURCE, &revision)?
                .map(serde_json::from_value::<Grant>)
                .transpose()
                .context("decode CapacityGrant")?
                .map(|grant| (grant.spec.fibers.max, grant.readiness()));
            Ok(Capacity { revision, grant })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Status {
        service: project.name.clone(),
        application_model: project.application_model().to_owned(),
        revision,
        url: service_status_url(&service, &installed)?,
        traffic,
        capacity,
    })
}

pub fn logs(
    directory: &Path,
    context: Option<&str>,
    tail: usize,
    audit: bool,
    host: &Host,
) -> Result<Vec<String>> {
    let project = Project::load(directory)?;
    validate_name(&project.name)?;
    let kube = Kube::new(directory, context, host)?;
    let grants: Vec<Grant> = kube.list(
        "default",
        grant::RESOURCE,
        &format!("serving.knative.dev/service={}", project.name),
    )?;
    let mut records = Vec::new();
    for grant in grants {
        let namespace = &grant.metadata.namespace;
        let pods: Vec<Value> = kube.list(namespace, "pods", &grant.pod_selector())?;
        for pod in grant::running_pods(&pods) {
            let mut command = kube.command(Some(namespace))?;
            command.args(read_args(&pod, audit));
            // A Pod that stops, or whose helper log isn't there yet before
            // its first warm, shouldn't hide what the others recorded.
            let data = match kube.capture(&mut command) {
                Ok(data) => data,
                Err(error) if host.cancellation.is_cancelled() => return Err(error),
                Err(error) => {
                    eprintln!("warning: skipped grant Pod {}: {error:#}", pod.name);
                    continue;
                }
            };
            let lines: Vec<&str> = data.lines().collect();
            records.extend(last(
                lines[lines.len().saturating_sub(LOG_LINES)..]
                    .iter()
                    .filter(|line| selected(line, audit, pod.uid, &project.name))
                    .map(|line| (*line).to_owned())
                    .collect(),
                tail,
            ));
        }
    }
    Ok(records)
}

/// The last `tail` of one grant Pod's `records`, or all of them for 0.
/// Each Pod is cut on its own: Pods' records interleave in time, so the
/// last of their concatenation would be the last Pod's alone.
fn last(mut records: Vec<String>, tail: usize) -> Vec<String> {
    if tail > 0 && records.len() > tail {
        records.drain(..records.len() - tail);
    }
    records
}

pub fn scale(directory: &Path, context: Option<&str>, max: i32, host: &Host) -> Result<String> {
    ensure!(max > 0, "max fibers must be positive");
    // The home's memory ceiling is 16 MiB per Fiber, capped at 64 GiB.
    ensure!(
        max <= MAX_FIBERS,
        "max fibers must be at most {MAX_FIBERS}: one home runs every Fiber of a Revision"
    );
    let project = Project::load(directory)?;
    validate_name(&project.name)?;
    let kube = Kube::new(directory, context, host)?;
    let service: Value = kube.get("default", "service.serving.knative.dev", &project.name)?;
    let name = active_revision(&service)?;
    let revision: Value = kube.get("default", "revision.serving.knative.dev", &name)?;
    let previous = revision
        .pointer("/metadata/annotations/autoscaling.knative.dev~1max-scale")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let requested = max.to_string();
    patch_revision_scale(&kube, &name, Some(&requested))?;
    let changed = grant::wait(&kube, &name, max).map(drop);
    if let Err(error) = changed {
        let rollback_host = Host {
            verbose: host.verbose,
            cancellation: Default::default(),
        };
        let rollback_kube = Kube::new(directory, Some(&kube.context), &rollback_host)?;
        let rollback = patch_revision_scale(&rollback_kube, &name, previous.as_deref());
        if let Err(rollback) = rollback {
            anyhow::bail!("{error:#}; capacity rollback: {rollback:#}");
        }
        return Err(error);
    }
    Ok(name)
}

/// The kubectl arguments that read grant Pod `pod`'s audit spool or helper
/// log. Neither reaches the container log: fiberd sends the helper's
/// stdout and stderr, which carry the handlers' records, to zygote.log in
/// the grant's run directory.
fn read_args(pod: &grant::RunningPod<'_>, audit: bool) -> Vec<String> {
    let mut args = [
        "exec",
        &format!("pod/{}", pod.name),
        "--container",
        "fiberd",
        "--",
    ]
    .map(str::to_owned)
    .to_vec();
    args.extend([
        "tail".to_owned(),
        LOG_BYTES.to_owned(),
        if audit {
            format!("{STATE_DIR}/private/audit.jsonl")
        } else {
            format!("{STATE_DIR}/run/{}/zygote.log", pod.uid)
        },
    ]);
    args
}

/// Reports whether a grant Pod's log or audit `line` belongs to the
/// Service: an audit record fiberd stamped with the Pod's grant UID, a gap
/// or checkpoint record of the Pod's spool, or a record the Service's
/// handler wrote.
fn selected(line: &str, audit: bool, pod_uid: &str, service: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return false;
    };
    if audit {
        // The spool writes these itself, with no fence. A gap says records
        // are missing and a checkpoint signs the chain, so dropping them
        // would hide both.
        matches!(
            value.get("event").and_then(Value::as_str),
            Some("gap" | "checkpoint")
        ) || value.pointer("/fence/GrantUID").and_then(Value::as_str) == Some(pod_uid)
    } else {
        value.get("kind").and_then(Value::as_str) == Some("handler")
            && value.get("service").and_then(Value::as_str) == Some(service)
    }
}

fn active_revision(service: &Value) -> Result<String> {
    let active = active_revisions(service)?;
    ensure!(
        active.len() == 1,
        "Knative Service has {} active traffic Revisions; scale requires exactly one",
        active.len()
    );
    active
        .into_iter()
        .next()
        .context("Knative Service has no active traffic Revision")
}

fn active_revisions(service: &Value) -> Result<BTreeSet<String>> {
    let targets = service
        .pointer("/status/traffic")
        .and_then(Value::as_array)
        .context("Knative Service has no resolved traffic")?;
    let active = targets
        .iter()
        .filter(|target| target.get("percent").and_then(Value::as_i64).unwrap_or(0) > 0)
        .map(|target| {
            target
                .get("revisionName")
                .and_then(Value::as_str)
                .context("Knative traffic target has no Revision")
                .map(str::to_owned)
        })
        .collect::<Result<BTreeSet<_>>>()?;
    ensure!(
        !active.is_empty(),
        "Knative Service has no active traffic Revisions"
    );
    Ok(active)
}

pub(crate) fn observed_service_url(service: &Value) -> Result<Url> {
    let name = service
        .pointer("/metadata/name")
        .and_then(Value::as_str)
        .context("Knative Service has no name")?;
    let namespace = service
        .pointer("/metadata/namespace")
        .and_then(Value::as_str)
        .context("Knative Service has no namespace")?;
    let observed = service
        .pointer("/status/url")
        .and_then(Value::as_str)
        .context("Knative Service has no ready URL")?;
    let url = Url::parse(observed).context("Knative Service has an invalid URL")?;
    ensure!(
        matches!(url.scheme(), "http" | "https"),
        "Knative Service URL must use HTTP or HTTPS"
    );
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "Knative Service URL cannot contain credentials"
    );
    ensure!(
        url.path() == "/" && url.query().is_none() && url.fragment().is_none(),
        "Knative Service URL must not contain a path, query or fragment"
    );
    let host = url.host_str().context("Knative Service URL has no host")?;
    ensure!(
        host.starts_with(&format!("{name}.{namespace}.")),
        "Knative Service URL does not match the Service"
    );
    Ok(url)
}

pub(crate) fn service_status_url(service: &Value, installation: &Installation) -> Result<String> {
    let mut url = observed_service_url(service)?;
    if kube::cluster_local(service) {
        ensure!(
            url.scheme() == "http"
                && url
                    .host_str()
                    .is_some_and(|host| host.ends_with(".svc.cluster.local")),
            "cluster-local Knative Service URL is not internal HTTP"
        );
        let mut rendered = url.to_string();
        rendered.truncate(rendered.trim_end_matches('/').len());
        return Ok(rendered);
    }
    ensure!(
        !url.host_str()
            .is_some_and(|host| host.ends_with(".svc.cluster.local")),
        "public Knative Service URL is cluster-local"
    );
    if installation.domain.ends_with(".invalid") {
        return Ok(String::new());
    }
    url.set_scheme("https")
        .map_err(|()| anyhow::anyhow!("Knative Service URL cannot use HTTPS"))?;
    match installation.https_port.as_deref() {
        Some("443") | None => {
            url.set_port(None)
                .map_err(|()| anyhow::anyhow!("Knative Service URL cannot use the HTTPS port"))?;
        }
        Some(port) => {
            let port = port
                .parse::<u16>()
                .context("installation has an invalid HTTPS port")?;
            url.set_port(Some(port))
                .map_err(|()| anyhow::anyhow!("Knative Service URL cannot use the HTTPS port"))?;
        }
    }
    let mut rendered = url.to_string();
    if url.path() == "/" && url.query().is_none() && url.fragment().is_none() {
        rendered.truncate(rendered.trim_end_matches('/').len());
    }
    Ok(rendered)
}

fn patch_revision_scale(kube: &Kube<'_>, revision: &str, max: Option<&str>) -> Result<()> {
    let mut patch = json!({"metadata":{"annotations":{}}});
    patch["metadata"]["annotations"][MAX_SCALE_ANNOTATION] = max.map_or(Value::Null, Into::into);
    kube.patch("default", "revision.serving.knative.dev", revision, &patch)
}

pub fn delete(directory: &Path, context: Option<&str>, host: &Host) -> Result<()> {
    let project = Project::load(directory)?;
    validate_name(&project.name)?;
    let kube = Kube::new(directory, context, host)?;
    if let Some(service) = kube.optional("default", "service.serving.knative.dev", &project.name)? {
        kube::ensure_owner(&service, &project.name)?;
    }
    host.run(kube.command(Some("default"))?.args([
        "delete",
        "service.serving.knative.dev",
        &project.name,
        "--ignore-not-found=true",
        "--cascade=foreground",
        "--wait=true",
        "--timeout=60s",
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kube::Installation;
    use std::collections::BTreeMap;

    #[test]
    fn active_revision_uses_resolved_traffic() {
        let service = json!({"status":{
            "latestReadyRevisionName":"demo-00002",
            "traffic":[{"revisionName":"demo-00001","percent":100}]
        }});
        assert_eq!(active_revision(&service).unwrap(), "demo-00001");
        let split = json!({"status":{"traffic":[
            {"revisionName":"demo-00001","percent":50},
            {"revisionName":"demo-00002","percent":50}
        ]}});
        assert!(active_revision(&split).is_err());
        let tagged = json!({"status":{"traffic":[
            {"revisionName":"demo-00001","percent":50,"tag":"blue"},
            {"revisionName":"demo-00001","percent":50,"tag":"green"}
        ]}});
        assert_eq!(active_revision(&tagged).unwrap(), "demo-00001");
        let revisions = active_revisions(&split).unwrap();
        assert_eq!(
            revisions.into_iter().collect::<Vec<_>>(),
            ["demo-00001", "demo-00002"]
        );
    }

    #[test]
    fn status_reports_each_grant_as_the_controller_does() {
        let capacity = |grant| {
            Capacity {
                revision: "demo-00001".to_owned(),
                grant,
            }
            .describe()
        };
        assert_eq!(
            capacity(Some((100, Readiness::Ready))),
            "capacity demo-00001 ready, max 100 fibers"
        );
        assert_eq!(
            capacity(Some((
                200,
                Readiness::Pending("CapacityGrant is rolling out: ReplicaSetUpdated".to_owned())
            ))),
            "capacity demo-00001 pending, max 200 fibers: CapacityGrant is rolling out: ReplicaSetUpdated"
        );
        assert_eq!(
            capacity(Some((
                200,
                Readiness::Failed("CapacityGrant is refused: Forbidden".to_owned())
            ))),
            "capacity demo-00001 failed, max 200 fibers: CapacityGrant is refused: Forbidden"
        );
        assert_eq!(
            capacity(None),
            "capacity demo-00001 missing: no CapacityGrant yet"
        );
    }

    #[test]
    fn logs_keep_only_the_service_handler_and_its_grant_pod_audit_records() {
        let handler = r#"{"kind":"handler","service":"demo","message":"hi"}"#;
        assert!(selected(handler, false, "pod-1", "demo"));
        assert!(!selected(handler, false, "pod-1", "other"));
        assert!(!selected(
            r#"{"kind":"fiberd","service":"demo"}"#,
            false,
            "pod-1",
            "demo"
        ));
        assert!(!selected(
            "helper: closing a connection to port 8080: reset",
            false,
            "pod-1",
            "demo"
        ));
        // fiberd's Fence has no JSON tags, so its fields keep their Go names.
        // Its grant UID is the Pod's, never the CapacityGrant's.
        let audit = |uid: &str| {
            format!(
                r#"{{"seq":1,"event":"clone","fence":{{"GrantUID":"{uid}","Epoch":2,"Seq":1}}}}"#
            )
        };
        assert!(selected(&audit("pod-1"), true, "pod-1", "demo"));
        assert!(!selected(&audit("grant-1"), true, "pod-1", "demo"));
        assert!(!selected(handler, true, "pod-1", "demo"));
        for record in [
            r#"{"seq":7,"event":"gap","fence":{"GrantUID":"","Epoch":0,"Seq":0},"lost":{"from":5,"to":6}}"#,
            r#"{"seq":8,"event":"checkpoint","fence":{"GrantUID":"","Epoch":0,"Seq":0},"signer":"k1"}"#,
        ] {
            assert!(selected(record, true, "pod-1", "demo"), "{record}");
            assert!(!selected(record, false, "pod-1", "demo"), "{record}");
        }
    }

    #[test]
    fn tail_keeps_the_last_records_of_each_grant_pod() {
        let records = |count: usize| (0..count).map(|n| n.to_string()).collect::<Vec<_>>();
        assert_eq!(last(records(5), 2), ["3", "4"]);
        assert_eq!(last(records(2), 5), ["0", "1"]);
        assert_eq!(last(records(3), 0), ["0", "1", "2"]);
    }

    #[test]
    fn logs_read_the_helper_log_and_audit_spool_of_each_grant_pod() {
        let pod = grant::RunningPod {
            name: "fiberd-demo-00001-abc",
            uid: "pod-1",
        };
        let exec = [
            "exec",
            "pod/fiberd-demo-00001-abc",
            "--container",
            "fiberd",
            "--",
        ];
        assert_eq!(
            read_args(&pod, false),
            [
                &exec[..],
                &[
                    "tail",
                    "--bytes=7340032",
                    "/var/lib/fiberd/run/pod-1/zygote.log"
                ]
            ]
            .concat()
        );
        assert_eq!(
            read_args(&pod, true),
            [
                &exec[..],
                &[
                    "tail",
                    "--bytes=7340032",
                    "/var/lib/fiberd/private/audit.jsonl"
                ]
            ]
            .concat()
        );
    }

    #[test]
    fn status_url_preserves_observed_host_and_cluster_visibility() {
        let installation = Installation {
            data: BTreeMap::new(),
            domain: "localhost".to_owned(),
            https_port: Some("8443".to_owned()),
        };
        let external = json!({
            "metadata":{"name":"custom","namespace":"default"},
            "status":{"url":"http://custom.default.example.com"}
        });
        assert_eq!(
            service_status_url(&external, &installation).unwrap(),
            "https://custom.default.example.com:8443"
        );
        let internal = json!({
            "metadata":{
                "name":"demo",
                "namespace":"default",
                "labels":{"networking.knative.dev/visibility":"cluster-local"}
            },
            "status":{"url":"http://demo.default.svc.cluster.local"}
        });
        assert_eq!(
            service_status_url(&internal, &installation).unwrap(),
            "http://demo.default.svc.cluster.local"
        );
        let unrelated = json!({
            "metadata":{
                "name":"demo",
                "namespace":"default",
                "labels":{"networking.knative.dev/visibility":"cluster-local"}
            },
            "status":{"url":"ftp://other.other.svc.cluster.local"}
        });
        assert!(service_status_url(&unrelated, &installation).is_err());
        let wrong_service = json!({
            "metadata":{"name":"demo","namespace":"default"},
            "status":{"url":"http://other.other.example.com"}
        });
        assert!(service_status_url(&wrong_service, &installation).is_err());
        for observed in [
            "https://user@demo.default.example.com",
            "http://demo.default.example.com/path",
            "http://demo.default.example.com?query=true",
        ] {
            let invalid = json!({
                "metadata":{"name":"demo","namespace":"default"},
                "status":{"url":observed}
            });
            assert!(service_status_url(&invalid, &installation).is_err());
        }
        let public_internal = json!({
            "metadata":{"name":"demo","namespace":"default"},
            "status":{"url":"http://demo.default.svc.cluster.local"}
        });
        assert!(service_status_url(&public_internal, &installation).is_err());
        let secure_internal = json!({
            "metadata":{
                "name":"demo",
                "namespace":"default",
                "labels":{"networking.knative.dev/visibility":"cluster-local"}
            },
            "status":{"url":"https://demo.default.svc.cluster.local"}
        });
        assert!(service_status_url(&secure_internal, &installation).is_err());
    }
}
