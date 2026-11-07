// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

use crate::fiberd_template;
use crate::host::Host;
use crate::image::{self, Metadata};
use crate::kube::{self, Kube};
use crate::project::{Project, REVISION_SUFFIX_LENGTH, validate_service_name};
use crate::registry;
use crate::routing::Traffic;

pub struct Options<'a> {
    pub directory: &'a Path,
    pub artifact: Option<&'a Path>,
    pub image: &'a str,
    pub context: Option<&'a str>,
    pub namespace: &'a str,
    pub traffic: &'a str,
}

pub struct Deployed {
    pub service: String,
    pub revision: String,
    pub image: String,
    pub url: String,
    pub exposed: bool,
}

pub fn deploy(options: Options<'_>, host: &Host) -> Result<Deployed> {
    ensure!(
        options.namespace == "default",
        "deployment currently supports the default namespace"
    );
    let directory = options
        .directory
        .canonicalize()
        .context("resolve service directory")?;
    let project = Project::load(&directory)?;
    crate::adapter::diagnose(&directory, &project)?;
    // Before anything is published, since a longer name's Revision would
    // wait for a grant the serving controller can never create.
    validate_service_name(&project.name)?;
    let exposed = crate::customer_workspace::exposed_services(&directory)?
        .is_some_and(|services| services.contains(&project.name));
    let prepared = host.step("Checking your local artifact", || {
        registry::prepare(
            &registry::Options {
                directory: options.directory,
                artifact: options.artifact,
                image: options.image,
            },
            host,
        )
    })?;
    ensure!(
        project.name == prepared.metadata.project.name,
        "project does not match artifact metadata"
    );
    let mut ignored = vec![".git".into(), ".lampyr".into()];
    if let Ok(relative) = prepared.artifact.strip_prefix(&prepared.directory) {
        ignored.push(relative.to_owned());
    }
    ensure!(
        image::directory_digest(&prepared.directory, &ignored)? == prepared.metadata.source.digest,
        "project source changed after the artifact was built; run ly build again"
    );
    let kube = Kube::new(&prepared.directory, options.context, host)?;
    let installed = kube.installation()?;
    let revision = revision_name(&project.name, &prepared.metadata.revision_id)?;
    let traffic = crate::routing::traffic(options.traffic, &revision)?;
    let facts = fiberd_template::recorded_facts(&prepared.metadata)?;
    let existing_revision = kube.optional("default", "revision.serving.knative.dev", &revision)?;
    // Grant Pods run only fiberd templates, so a new Revision without one
    // would be refused by the serving controller after the push.
    ensure!(
        existing_revision.is_some() || facts.is_some(),
        "the artifact has no fiberd template; rebuild it with ly build where the platform has build/hluk-helper (in a checkout, run `just template-tools` first)"
    );
    // Every template of the installation goes to the one repository its
    // install recorded, which its GrantPolicy admits.
    let template_repository = existing_revision
        .is_none()
        .then(|| installed.required("templateRepository"))
        .transpose()?;
    let image = registry::ensure_published(&prepared, options.image, host)?;
    host.step("Verifying the published artifact", || {
        registry::verify_packaged(
            &prepared.directory,
            &image,
            &prepared.artifact,
            &prepared.metadata,
            host,
        )
    })?;
    let metadata_digest = image::file_digest(&prepared.artifact.join("metadata.json"))?;
    if let Some(existing) = &existing_revision {
        ensure!(
            existing
                .pointer("/metadata/labels/serving.knative.dev~1service")
                .and_then(Value::as_str)
                == Some(project.name.as_str())
                && existing
                    .pointer("/metadata/annotations/lampyr.dev~1snapshot-artifact")
                    .and_then(Value::as_str)
                    == Some(image.as_str())
                && existing
                    .pointer("/metadata/annotations/lampyr.dev~1metadata-digest")
                    .and_then(Value::as_str)
                    == Some(metadata_digest.as_str()),
            "immutable revision {revision:?} already references a different artifact"
        );
    }
    // A sealed template's digest changes with every seal, and a named
    // Revision's template cannot change, so an existing Revision keeps
    // whatever template it was created with.
    let template = match (&existing_revision, facts, template_repository) {
        (Some(existing), _, _) => fiberd_template::recorded(existing)?,
        (None, None, _) | (None, _, None) => None,
        (None, Some(facts), Some(repository)) => {
            Some(host.step("Sealing your fiberd template", || {
                fiberd_template::publish(
                    &kube,
                    &crate::service::platform_root(&prepared.directory, None)?,
                    &prepared.artifact,
                    &facts,
                    repository,
                    &revision,
                    host,
                )
            })?)
        }
    };
    for target in &traffic {
        if target.revision == revision {
            continue;
        }
        let existing: Value =
            kube.get("default", "revision.serving.knative.dev", &target.revision)?;
        ensure!(
            existing
                .pointer("/metadata/labels/serving.knative.dev~1service")
                .and_then(Value::as_str)
                == Some(project.name.as_str()),
            "traffic target belongs to another Knative Service"
        );
    }
    if let Some(existing) =
        kube.optional("default", "service.serving.knative.dev", &project.name)?
    {
        kube::ensure_owner(&existing, &project.name)?;
    }
    let observed = host.step("Configuring your Lampyr Service", || {
        let mut service = resources(
            &prepared.metadata,
            &metadata_digest,
            &image,
            &revision,
            &traffic,
            exposed,
        )?;
        keep_existing_timeout(&mut service, existing_revision.as_ref());
        if let Some(template) = &template {
            fiberd_template::annotate(&mut service, template)?;
        }
        kube.apply(&service)?;
        kube.patch(
            "default",
            "service.serving.knative.dev",
            &project.name,
            &json!({"metadata":{"labels":{
                kube::VISIBILITY_LABEL: if exposed { Value::Null } else { json!("cluster-local") }
            }}}),
        )?;
        wait_for_deployment(&kube, &project.name, &revision, exposed)
    })?;
    Ok(Deployed {
        service: project.name.clone(),
        revision,
        image,
        url: crate::lifecycle::service_status_url(&observed, &installed)?,
        exposed,
    })
}

/// Knative fills in `timeoutSeconds` when it creates a Revision, and rejects a
/// template that names an existing Revision but differs from it. Revisions
/// created before `ly deploy` stopped writing 30 seconds keep that value, so
/// reapplying one carries its timeout over, as the readiness probe default is.
pub fn keep_existing_timeout(service: &mut Value, existing: Option<&Value>) {
    if let Some(timeout) = existing
        .and_then(|revision| revision.pointer("/spec/timeoutSeconds"))
        .and_then(Value::as_i64)
    {
        service["spec"]["template"]["spec"]["timeoutSeconds"] = timeout.into();
    }
}

pub fn revision_name(service: &str, id: &str) -> Result<String> {
    validate_service_name(service)?;
    let id_length = REVISION_SUFFIX_LENGTH - 1;
    ensure!(id.len() >= id_length, "artifact Revision ID is too short");
    Ok(format!("{service}-{}", &id[..id_length]))
}

pub fn resources(
    metadata: &Metadata,
    digest: &str,
    image: &str,
    revision: &str,
    traffic: &[Traffic],
    exposed: bool,
) -> Result<Value> {
    let project = &metadata.project;
    let name = &project.name;
    let labels = kube::labels(name);
    let mut service_labels = labels.clone();
    if !exposed {
        service_labels[kube::VISIBILITY_LABEL] = json!("cluster-local");
    }
    let execution = project.effective_execution()?;
    let container_concurrency = 1;
    let mut annotations = serde_json::Map::from_iter([
        ("lampyr.dev/snapshot-artifact".into(), image.into()),
        (
            "lampyr.dev/revision-id".into(),
            metadata.revision_id.clone().into(),
        ),
        (
            "lampyr.dev/source-digest".into(),
            metadata.source.digest.clone().into(),
        ),
        ("lampyr.dev/metadata-digest".into(), digest.into()),
        (
            "lampyr.dev/execution-mode".into(),
            execution.mode.clone().into(),
        ),
        ("autoscaling.knative.dev/max-scale".into(), "100".into()),
    ]);
    if !execution.idle_timeout.is_empty() {
        annotations.insert(
            "lampyr.dev/idle-timeout".into(),
            execution.idle_timeout.into(),
        );
    }
    let traffic: Vec<_> = traffic
        .iter()
        .map(|target| {
            json!({
                "revisionName": target.revision,
                "percent": target.percent,
            })
        })
        .collect();
    // Preserve Knative's create-time probe default when reapplying a named Revision.
    let mut container = json!({
        "name": "user-container",
        "image": image,
        "readinessProbe": {
            "successThreshold": 1,
            "tcpSocket": {"port": 0},
        },
        "resources": {
            "requests": {
                "memory": project.resources.memory,
            },
        },
    });
    if project.protocols.grpc {
        container["ports"] = json!([{
            "name": "h2c",
            "containerPort": 8080,
            "protocol": "TCP",
        }]);
    }
    Ok(json!({
        "apiVersion": "serving.knative.dev/v1",
        "kind": "Service",
        "metadata": {
            "name": name,
            "namespace": "default",
            "labels": service_labels,
            "annotations": {
                "lampyr.dev/events-enabled": project.protocols.events.to_string(),
            },
        },
        "spec": {
            "template": {
                "metadata": {
                    "name": revision,
                    "labels": labels,
                    "annotations": annotations,
                },
                "spec": {
                    "containerConcurrency": container_concurrency,
                    "containers": [container],
                },
            },
            "traffic": traffic,
        },
    }))
}

fn wait_for_deployment(
    kube: &Kube<'_>,
    service_name: &str,
    revision_name: &str,
    exposed: bool,
) -> Result<Value> {
    kube.wait(
        "Knative Service deployment",
        Duration::from_secs(240),
        || {
            let Some(service) =
                kube.optional("default", "service.serving.knative.dev", service_name)?
            else {
                return Ok(None);
            };
            let Some(revision) =
                kube.optional("default", "revision.serving.knative.dev", revision_name)?
            else {
                return Ok(None);
            };
            let Some(route) =
                kube.optional("default", "route.serving.knative.dev", service_name)?
            else {
                return Ok(None);
            };
            Ok((deployment_ready(&service, &revision, revision_name)
                && ready(&route)
                && visibility_ready(&service, &route, exposed)?)
            .then_some(service))
        },
    )
}

fn visibility_ready(service: &Value, route: &Value, exposed: bool) -> Result<bool> {
    for resource in [service, route] {
        let url = crate::lifecycle::observed_service_url(resource)?;
        let internal = url
            .host_str()
            .is_some_and(|host| host.ends_with(".svc.cluster.local"));
        ensure!(
            !(kube::cluster_local(resource) && internal) || url.scheme() == "http",
            "cluster-local Knative Service URL must use HTTP"
        );
        if kube::cluster_local(resource) == exposed || internal == exposed {
            return Ok(false);
        }
    }
    Ok(true)
}

fn deployment_ready(service: &Value, revision: &Value, revision_name: &str) -> bool {
    let generation = service["metadata"]["generation"].as_i64();
    let observed = service["status"]["observedGeneration"].as_i64();
    generation.is_some()
        && observed.is_some_and(|observed| Some(observed) >= generation)
        && service["status"]["latestCreatedRevisionName"] == revision_name
        && service["status"]["latestReadyRevisionName"] == revision_name
        && ready(service)
        && ready(revision)
}

fn ready(resource: &Value) -> bool {
    resource["status"]["conditions"]
        .as_array()
        .is_some_and(|conditions| {
            conditions
                .iter()
                .any(|condition| condition["type"] == "Ready" && condition["status"] == "True")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visibility_waits_for_service_and_route_urls_after_label_only_changes() {
        let public = json!({
            "metadata":{"name":"demo","namespace":"default","labels":{}},
            "status":{"url":"http://demo.default.example.com"}
        });
        let internal = json!({
            "metadata":{
                "name":"demo",
                "namespace":"default",
                "labels":{kube::VISIBILITY_LABEL:"cluster-local"}
            },
            "status":{"url":"http://demo.default.svc.cluster.local"}
        });
        assert!(visibility_ready(&public, &public, true).unwrap());
        assert!(visibility_ready(&internal, &internal, false).unwrap());
        assert!(!visibility_ready(&public, &internal, true).unwrap());
        assert!(!visibility_ready(&internal, &public, false).unwrap());
        let mut stale = internal.clone();
        stale["metadata"]["labels"] = json!({});
        assert!(!visibility_ready(&stale, &stale, true).unwrap());
        let mut stale = public.clone();
        stale["metadata"]["labels"] = internal["metadata"]["labels"].clone();
        assert!(!visibility_ready(&stale, &stale, false).unwrap());
        assert!(visibility_ready(&json!({}), &public, true).is_err());
        let mut secure_internal = internal.clone();
        secure_internal["status"]["url"] = json!("https://demo.default.svc.cluster.local");
        assert!(visibility_ready(&secure_internal, &secure_internal, false).is_err());
    }

    #[test]
    fn deployment_readiness_requires_current_generation_and_revision() {
        let revision = json!({
            "status":{"conditions":[{"type":"Ready","status":"True"}]}
        });
        let service = |observed: i64, latest: &str| {
            json!({
                "metadata":{"generation":2},
                "status":{
                    "observedGeneration":observed,
                    "latestCreatedRevisionName":latest,
                    "latestReadyRevisionName":latest,
                    "conditions":[{"type":"Ready","status":"True"}]
                }
            })
        };
        assert!(!deployment_ready(
            &service(1, "demo-00002"),
            &revision,
            "demo-00002"
        ));
        assert!(!deployment_ready(
            &service(2, "demo-00001"),
            &revision,
            "demo-00002"
        ));
        assert!(deployment_ready(
            &service(2, "demo-00002"),
            &revision,
            "demo-00002"
        ));
    }

    #[test]
    fn revision_names_retain_the_complete_service_name() {
        let service = "a".repeat(43);
        let revision = revision_name(&service, "0123456789abcdef").unwrap();
        assert_eq!(revision, format!("{service}-0123456789ab"));
        // The serving controller's CapacityGrant name limit.
        assert_eq!(revision.len(), 56);
        let error = revision_name(&"a".repeat(44), "0123456789abcdef").unwrap_err();
        assert!(error.to_string().contains("at most 43"), "{error:#}");
    }
}
