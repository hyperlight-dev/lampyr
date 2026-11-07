// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::artifact::{Operation, SourceKind};
use crate::host::Host;
use crate::installation::Installation;
use crate::kube::{Kube, SYSTEM};
use crate::project::Project;

const INVENTORY_LIMIT: usize = 512 * 1024;
const MAX_MANIFESTS: usize = 32;

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SourcePlan {
    #[serde(default)]
    shared_manifests: Vec<Value>,
    manifests: Vec<Value>,
    observations: Vec<Value>,
    timeout_seconds: u64,
}

fn delete_owned(kube: &Kube<'_>, record: &SourceRecord, plan: &DeletePlan) -> Result<()> {
    ensure!(
        plan.manifests.len() <= record.plan.manifests.len(),
        "provider event source deletion plan exceeds its inventory"
    );
    let owned: BTreeMap<_, _> = record
        .plan
        .manifests
        .iter()
        .map(|manifest| Ok((identity(manifest)?, manifest)))
        .collect::<Result<_>>()?;
    for manifest in &plan.manifests {
        crate::cluster::validate_manifest(manifest)?;
        ensure!(
            manifest["metadata"]["namespace"] == record.namespace,
            "provider event source deletion escaped its namespace"
        );
        let key = identity(manifest)?;
        ensure!(
            owned.contains_key(&key),
            "provider event source deletion escaped its recorded inventory"
        );
        kube.delete_manifest(manifest)?;
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DeletePlan {
    manifests: Vec<Value>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SourceStatus {
    ready: bool,
    reason: String,
    message: String,
    terminal: bool,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SourceRecord {
    schema: u32,
    provider: String,
    installation_id: String,
    namespace: String,
    name: String,
    service: String,
    plan: SourcePlan,
}

pub fn run(
    operation: Operation,
    provider: &str,
    directory: &Path,
    context: Option<&str>,
    namespace: &str,
    source_name: &str,
    host: &Host,
) -> Result<()> {
    ensure!(
        matches!(
            operation,
            Operation::SourceCreate | Operation::SourceStatus | Operation::SourceDelete
        ),
        "invalid event source operation"
    );
    crate::project::validate_name(provider)?;
    crate::project::validate_name(namespace)?;
    let project = Project::load(directory)?;
    let name = source_name.to_owned();
    crate::project::validate_name(&name)?;
    if operation == Operation::SourceCreate {
        ensure!(
            project.protocols.events,
            "events are not enabled for Lampyr Service {}",
            project.name
        );
    }
    let root = crate::service::platform_root(directory, None)?;
    let kube = Kube::new(&root, context, host)?;
    let metadata = kube.installation()?;
    let cluster_provider = metadata
        .data
        .get("provider")
        .map(String::as_str)
        .unwrap_or_default();
    ensure!(
        cluster_provider == provider,
        "event source provider {provider} does not own this {cluster_provider} installation"
    );
    ensure!(
        metadata
            .data
            .get("eventing")
            .is_some_and(|value| value == "true"),
        "cluster Eventing is not enabled; reinstall with --eventing"
    );
    let cluster = metadata
        .data
        .get("cluster")
        .context("installation cluster name is missing")?;
    let installation_id = metadata
        .data
        .get("installationId")
        .context("installation identity is missing")?;
    let installation = Installation::open_with_cancellation(
        &root,
        provider,
        cluster,
        None,
        None,
        host.cancellation.clone(),
    )?;
    ensure!(
        installation.record.id == *installation_id,
        "event source provider installation identity does not match the cluster"
    );
    if operation == Operation::SourceCreate {
        let service: Value = kube.get(namespace, "service.serving.knative.dev", &project.name)?;
        ensure!(
            service
                .pointer("/metadata/annotations/lampyr.dev~1events-enabled")
                .and_then(Value::as_str)
                == Some("true"),
            "events are not enabled for deployed Lampyr Service {}",
            project.name
        );
    }
    let previous = read_record(&kube, namespace, &name)?;
    if let Some(record) = &previous {
        ensure!(
            record.provider == provider
                && record.installation_id == *installation_id
                && record.namespace == namespace
                && record.name == name
                && record.service == project.name,
            "event source inventory belongs to another installation or service"
        );
        ensure_existing_owned(&kube, record)?;
    }
    let previous_observations = match &previous {
        Some(record) => observations(&kube, &record.plan.observations)?,
        None => Vec::new(),
    };
    let request = |observations: Vec<Value>| {
        json!({
            "name": name,
            "namespace": namespace,
            "service": project.name,
            "observations": observations,
        })
    };
    let result = match operation {
        Operation::SourceCreate => {
            let mut source_provider = installation.provider(operation, host)?;
            let source_kinds = source_provider.source_kinds().clone();
            validate_source_kinds(&installation, &source_kinds)?;
            let mut plan: SourcePlan = serde_json::from_value(installation.invoke_with(
                &mut source_provider,
                operation,
                request(previous_observations),
                host,
            )?)
            .context("decode provider event source plan")?;
            validate_plan(
                &mut plan,
                &source_kinds,
                provider,
                installation_id,
                namespace,
                &name,
                &project.name,
            )?;
            drop(source_provider);
            let record = SourceRecord {
                schema: 1,
                provider: provider.to_owned(),
                installation_id: installation_id.to_owned(),
                namespace: namespace.to_owned(),
                name: name.clone(),
                service: project.name.clone(),
                plan,
            };
            ensure_existing_owned(&kube, &record)?;
            save_record(&kube, &record)?;
            for manifest in record
                .plan
                .shared_manifests
                .iter()
                .chain(&record.plan.manifests)
            {
                kube.apply(manifest)?;
            }
            let deadline = Instant::now()
                .checked_add(Duration::from_secs(record.plan.timeout_seconds))
                .context("event source readiness deadline is out of range")?;
            let mut status_provider = installation.provider(Operation::SourceStatus, host)?;
            loop {
                ensure_existing_owned(&kube, &record)?;
                let observed = observations(&kube, &record.plan.observations)?;
                let remaining = readiness_remaining(deadline, &name, record.plan.timeout_seconds)?;
                let status: SourceStatus =
                    serde_json::from_value(installation.invoke_with_timeout(
                        &mut status_provider,
                        Operation::SourceStatus,
                        request(observed),
                        host,
                        remaining,
                    )?)
                    .context("decode provider event source status")?;
                validate_status(&status)?;
                if status.ready {
                    break serde_json::to_value(status)?;
                }
                ensure!(
                    !status.terminal,
                    "event source cannot become ready: {}: {}",
                    status.reason,
                    status.message
                );
                ensure!(
                    !host.cancellation.is_cancelled(),
                    "event source readiness cancelled"
                );
                std::thread::sleep(
                    Duration::from_secs(5).min(deadline.saturating_duration_since(Instant::now())),
                );
            }
        }
        Operation::SourceStatus => {
            let record = previous.context("event source is not installed")?;
            let status: SourceStatus = serde_json::from_value(installation.invoke(
                operation,
                request(observations(&kube, &record.plan.observations)?),
                host,
            )?)
            .context("decode provider event source status")?;
            validate_status(&status)?;
            serde_json::to_value(status)?
        }
        Operation::SourceDelete => {
            let record = previous.context("event source is not installed")?;
            let plan: DeletePlan = serde_json::from_value(installation.invoke(
                operation,
                request(observations(&kube, &record.plan.observations)?),
                host,
            )?)
            .context("decode provider event source deletion plan")?;
            delete_owned(&kube, &record, &plan)?;
            delete_record(&kube, namespace, &name)?;
            json!({"ready":false,"reason":"Deleted","message":"Event source deleted.","terminal":false})
        }
        _ => unreachable!(),
    };
    let mut result = result;
    result["name"] = name.into();
    println!("{}", serde_json::to_string(&result)?);
    Ok(())
}

fn readiness_remaining(deadline: Instant, name: &str, timeout_seconds: u64) -> Result<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    ensure!(
        !remaining.is_zero(),
        "event source {name} did not become ready within {timeout_seconds} seconds"
    );
    Ok(remaining)
}

pub(crate) fn uninstall_all(
    kube: &Kube<'_>,
    installation: &Installation,
    host: &Host,
) -> Result<()> {
    if kube.optional("default", "namespace", SYSTEM)?.is_none() {
        return Ok(());
    }
    let selector = format!(
        "app.kubernetes.io/managed-by=lampyr,lampyr.dev/installation-id={}",
        installation.record.id
    );
    let secrets: Vec<Value> = kube.list(SYSTEM, "secrets", &selector)?;
    let mut records = Vec::new();
    for secret in secrets {
        let Some(data) = secret["data"]["source"].as_str() else {
            continue;
        };
        let data = STANDARD.decode(data)?;
        ensure!(
            data.len() <= INVENTORY_LIMIT,
            "event source inventory exceeds 512 KiB"
        );
        let record: SourceRecord = serde_json::from_slice(&data)
            .context("decode event source inventory during uninstall")?;
        ensure!(
            record.schema == 1
                && record.provider == installation.record.provider
                && record.installation_id == installation.record.id,
            "event source inventory belongs to another provider installation"
        );
        records.push(record);
    }
    ensure_no_untracked_azure_sources(kube, &records)?;
    if records.is_empty() {
        return Ok(());
    }
    let mut provider = installation.provider(Operation::SourceDelete, host)?;
    for record in records {
        ensure_existing_owned(kube, &record)?;
        let observed = observations(kube, &record.plan.observations)?;
        let plan: DeletePlan = serde_json::from_value(installation.invoke_with(
            &mut provider,
            Operation::SourceDelete,
            json!({
                "name":record.name,
                "namespace":record.namespace,
                "service":record.service,
                "observations":observed,
            }),
            host,
        )?)
        .context("decode provider event source deletion plan")?;
        delete_owned(kube, &record, &plan)?;
        delete_record(kube, &record.namespace, &record.name)?;
    }
    Ok(())
}

pub(crate) fn ensure_providerless_uninstall_safe(
    kube: &Kube<'_>,
    installation_id: &str,
) -> Result<()> {
    let selector =
        format!("app.kubernetes.io/managed-by=lampyr,lampyr.dev/installation-id={installation_id}");
    let secrets: Vec<Value> = kube.list(SYSTEM, "secrets", &selector)?;
    let sources: Vec<_> = secrets
        .iter()
        .filter(|secret| secret["data"]["source"].is_string())
        .filter_map(|secret| secret["metadata"]["name"].as_str())
        .collect();
    ensure!(
        sources.is_empty(),
        "event source inventory remains ({}); rerun uninstall with the installation provider so source finalizers can complete",
        sources.join(", ")
    );

    const RESOURCE: &str = "azureeventgridsources.sources.lampyr.dev";
    if kube.optional("default", "crd", RESOURCE)?.is_none() {
        return Ok(());
    }
    let listing = kube.capture(kube.command(None)?.args([
        "get",
        RESOURCE,
        "--all-namespaces",
        "--output=json",
    ]))?;
    let listing: Value =
        serde_json::from_str(&listing).context("decode live provider Event Sources")?;
    let live: Vec<_> = listing["items"]
        .as_array()
        .context("provider Event Source list has no items")?
        .iter()
        .filter_map(|item| {
            Some(format!(
                "{}/{}",
                item["metadata"]["namespace"].as_str()?,
                item["metadata"]["name"].as_str()?,
            ))
        })
        .collect();
    ensure!(
        live.is_empty(),
        "live provider Event Sources remain ({}); rerun uninstall with the installation provider so finalizers can complete",
        live.join(", ")
    );
    Ok(())
}

fn ensure_no_untracked_azure_sources(kube: &Kube<'_>, records: &[SourceRecord]) -> Result<()> {
    const RESOURCE: &str = "azureeventgridsources.sources.lampyr.dev";
    if kube.optional("default", "crd", RESOURCE)?.is_none() {
        return Ok(());
    }
    let listing = kube.capture(kube.command(None)?.args([
        "get",
        RESOURCE,
        "--all-namespaces",
        "--output=json",
    ]))?;
    let listing: Value =
        serde_json::from_str(&listing).context("decode live Azure Event Grid sources")?;
    let untracked = untracked_azure_sources(records, &listing)?;
    ensure!(
        untracked.is_empty(),
        "untracked AzureEventGridSource resources remain: {}; delete them before uninstalling the Azure source platform",
        untracked.join(", ")
    );
    Ok(())
}

fn untracked_azure_sources(records: &[SourceRecord], listing: &Value) -> Result<Vec<String>> {
    let tracked: BTreeSet<_> = records
        .iter()
        .map(|record| (record.namespace.as_str(), record.name.as_str()))
        .collect();
    Ok(listing["items"]
        .as_array()
        .context("Azure Event Grid source list must contain items")?
        .iter()
        .filter_map(|source| {
            let namespace = source["metadata"]["namespace"].as_str()?;
            let name = source["metadata"]["name"].as_str()?;
            (!tracked.contains(&(namespace, name))).then(|| format!("{namespace}/{name}"))
        })
        .collect())
}

fn validate_plan(
    plan: &mut SourcePlan,
    source_kinds: &BTreeSet<SourceKind>,
    provider: &str,
    installation_id: &str,
    namespace: &str,
    name: &str,
    service: &str,
) -> Result<()> {
    ensure!(
        !plan.manifests.is_empty()
            && plan.manifests.len() <= MAX_MANIFESTS
            && plan.shared_manifests.len() <= MAX_MANIFESTS
            && !plan.observations.is_empty()
            && plan.observations.len() <= MAX_MANIFESTS,
        "provider event source plan has invalid resource counts"
    );
    for manifest in plan.shared_manifests.iter().chain(&plan.manifests) {
        let resource = SourceKind {
            api_version: manifest["apiVersion"]
                .as_str()
                .context("provider Event Source manifest apiVersion is missing")?
                .to_owned(),
            kind: manifest["kind"]
                .as_str()
                .context("provider Event Source manifest kind is missing")?
                .to_owned(),
        };
        ensure!(
            source_kinds.contains(&resource),
            "provider Event Source plan contains undeclared kind {}/{}",
            resource.api_version,
            resource.kind
        );
    }
    ensure!(
        (1..=3600).contains(&plan.timeout_seconds),
        "provider event source timeout must be 1 to 3600 seconds"
    );
    for manifest in &mut plan.shared_manifests {
        validate_source_manifest_policy(manifest, true, namespace, name, service)?;
        claim(manifest, provider, installation_id, None, None)?;
        ensure!(
            manifest["metadata"]["namespace"] == namespace,
            "provider shared event source manifest escaped its namespace"
        );
    }
    for manifest in &mut plan.manifests {
        validate_source_manifest_policy(manifest, false, namespace, name, service)?;
        claim(
            manifest,
            provider,
            installation_id,
            Some(name),
            Some(service),
        )?;
        ensure!(
            manifest["metadata"]["namespace"] == namespace,
            "provider event source manifest escaped its namespace"
        );
    }
    let identities: BTreeMap<_, _> = plan
        .shared_manifests
        .iter()
        .chain(&plan.manifests)
        .map(|manifest| Ok((identity(manifest)?, ())))
        .collect::<Result<_>>()?;
    for manifest in &plan.observations {
        crate::cluster::validate_manifest(manifest)?;
        ensure!(
            manifest["metadata"]["namespace"] == namespace
                && identities.contains_key(&identity(manifest)?),
            "provider event source observation escaped its manifest plan"
        );
    }
    Ok(())
}

fn validate_source_kinds(
    installation: &Installation,
    source_kinds: &BTreeSet<SourceKind>,
) -> Result<()> {
    let runtime = installation
        .host_state
        .read_state("runtime")?
        .context("provider installation has no approved Kubernetes runtime plan")?;
    let approved = approved_custom_source_kinds(&runtime)?;
    for resource in source_kinds {
        let safe_knative = resource.api_version == "eventing.knative.dev/v1"
            && matches!(resource.kind.as_str(), "Broker" | "Trigger");
        ensure!(
            safe_knative || approved.contains(resource),
            "provider Event Source kind {}/{} is not installed by its approved Kubernetes plan",
            resource.api_version,
            resource.kind
        );
    }
    Ok(())
}

fn approved_custom_source_kinds(runtime: &Value) -> Result<BTreeSet<SourceKind>> {
    let mut approved = BTreeSet::new();
    for phase in ["before", "after"] {
        let manifests = runtime[phase]
            .as_array()
            .context("approved provider runtime plan is invalid")?;
        for manifest in manifests {
            if manifest["apiVersion"] != "apiextensions.k8s.io/v1"
                || manifest["kind"] != "CustomResourceDefinition"
                || manifest["spec"]["scope"] != "Namespaced"
            {
                continue;
            }
            let group = manifest["spec"]["group"]
                .as_str()
                .context("provider CRD group is missing")?;
            let kind = manifest["spec"]["names"]["kind"]
                .as_str()
                .context("provider CRD kind is missing")?;
            let versions = manifest["spec"]["versions"]
                .as_array()
                .context("provider CRD versions are missing")?;
            for version in versions {
                if version["served"] == true {
                    let name = version["name"]
                        .as_str()
                        .context("provider CRD version name is missing")?;
                    approved.insert(SourceKind {
                        api_version: format!("{group}/{name}"),
                        kind: kind.to_owned(),
                    });
                }
            }
        }
    }
    Ok(approved)
}

fn validate_source_manifest_policy(
    manifest: &Value,
    shared: bool,
    namespace: &str,
    source: &str,
    service: &str,
) -> Result<()> {
    match (manifest["apiVersion"].as_str(), manifest["kind"].as_str()) {
        (Some("eventing.knative.dev/v1"), Some("Broker")) => {
            ensure!(
                shared
                    && manifest["metadata"]["name"] == "default"
                    && manifest["metadata"]["namespace"] == namespace
                    && manifest["spec"]
                        .as_object()
                        .is_some_and(serde_json::Map::is_empty),
                "provider Broker must be the shared empty default Broker"
            );
        }
        (Some("eventing.knative.dev/v1"), Some("Trigger")) => {
            ensure!(!shared, "provider Trigger cannot be shared");
            let spec = manifest["spec"]
                .as_object()
                .context("provider Trigger spec must be an object")?;
            ensure!(
                spec.keys()
                    .all(|key| matches!(key.as_str(), "broker" | "filter" | "subscriber"))
                    && spec.get("broker") == Some(&Value::String("default".into())),
                "provider Trigger must use only the default Broker"
            );
            let subscriber = spec
                .get("subscriber")
                .context("provider Trigger subscriber is missing")?
                .as_object()
                .context("provider Trigger subscriber must be an object")?;
            ensure!(
                subscriber
                    .keys()
                    .all(|key| matches!(key.as_str(), "ref" | "uri"))
                    && subscriber.get("uri") == Some(&Value::String("/events/knative".into())),
                "provider Trigger subscriber must use the Lampyr events path"
            );
            let reference = subscriber
                .get("ref")
                .context("provider Trigger subscriber ref is missing")?
                .as_object()
                .context("provider Trigger subscriber ref must be an object")?;
            ensure!(
                reference.keys().all(|key| {
                    matches!(key.as_str(), "apiVersion" | "kind" | "namespace" | "name")
                }) && reference.get("apiVersion")
                    == Some(&Value::String("serving.knative.dev/v1".into()))
                    && reference.get("kind") == Some(&Value::String("Service".into()))
                    && reference.get("name") == Some(&Value::String(service.into()))
                    && reference
                        .get("namespace")
                        .is_none_or(|value| value == namespace)
                    && manifest["metadata"]["name"] == source
                    && manifest["metadata"]["namespace"] == namespace,
                "provider Trigger subscriber must target the selected Knative Service"
            );
        }
        _ => {
            ensure!(!shared, "provider custom source resources cannot be shared");
            ensure!(
                manifest["metadata"]["name"] == source
                    && manifest["metadata"]["namespace"] == namespace,
                "provider custom source must match the selected source identity"
            );
            let sink = manifest["spec"]["sink"]
                .as_object()
                .context("provider custom source sink must be an object")?;
            ensure!(
                sink.keys().all(|key| key == "ref"),
                "provider custom source sink must contain only an object reference"
            );
            let reference = sink
                .get("ref")
                .context("provider custom source sink reference is missing")?
                .as_object()
                .context("provider custom source sink reference must be an object")?;
            ensure!(
                reference.keys().all(|key| {
                    matches!(key.as_str(), "apiVersion" | "kind" | "namespace" | "name")
                }) && reference.get("apiVersion")
                    == Some(&Value::String("eventing.knative.dev/v1".into()))
                    && reference.get("kind") == Some(&Value::String("Broker".into()))
                    && reference.get("name") == Some(&Value::String("default".into()))
                    && reference
                        .get("namespace")
                        .is_none_or(|value| value == namespace),
                "provider custom source sink must target the selected namespace's default Broker"
            );
        }
    }
    Ok(())
}

fn claim(
    manifest: &mut Value,
    provider: &str,
    installation_id: &str,
    source: Option<&str>,
    service: Option<&str>,
) -> Result<()> {
    crate::cluster::validate_manifest(manifest)?;
    let labels = manifest["metadata"]
        .as_object_mut()
        .context("provider manifest metadata must be an object")?
        .entry("labels")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("provider manifest labels must be an object")?;
    for (key, value) in [
        ("app.kubernetes.io/managed-by", Some("lampyr")),
        ("lampyr.dev/installation-id", Some(installation_id)),
        ("events.lampyr.dev/provider", Some(provider)),
        ("events.lampyr.dev/source", source),
        ("edge.lampyr.dev/service", service),
    ] {
        let Some(value) = value else { continue };
        ensure!(
            labels.get(key).is_none_or(|current| current == value),
            "provider manifest conflicts with Lampyr ownership label {key}"
        );
        labels.insert(key.into(), value.into());
    }
    Ok(())
}

fn ensure_existing_owned(kube: &Kube<'_>, record: &SourceRecord) -> Result<()> {
    for (manifest, shared) in record
        .plan
        .shared_manifests
        .iter()
        .map(|manifest| (manifest, true))
        .chain(
            record
                .plan
                .manifests
                .iter()
                .map(|manifest| (manifest, false)),
        )
    {
        let Some(existing) = kube.manifest_optional(manifest)? else {
            continue;
        };
        let labels = &existing["metadata"]["labels"];
        ensure!(
            labels["app.kubernetes.io/managed-by"] == "lampyr"
                && labels["lampyr.dev/installation-id"] == record.installation_id
                && labels["events.lampyr.dev/provider"] == record.provider,
            "refusing to replace an event source resource owned by another installation"
        );
        if !shared {
            ensure!(
                labels["events.lampyr.dev/source"] == record.name
                    && labels["edge.lampyr.dev/service"] == record.service,
                "refusing to replace a resource owned by another event source"
            );
        }
    }
    Ok(())
}

fn observations(kube: &Kube<'_>, manifests: &[Value]) -> Result<Vec<Value>> {
    manifests
        .iter()
        .map(|identity| {
            let object = kube
                .manifest_optional(identity)?
                .map(|object| observation_object(identity, object))
                .transpose()?;
            Ok(json!({
                "identity": identity,
                "object": object,
            }))
        })
        .collect()
}

fn observation_object(identity: &Value, object: Value) -> Result<Value> {
    ensure!(
        object["apiVersion"] == identity["apiVersion"]
            && object["kind"] == identity["kind"]
            && object["metadata"]["namespace"] == identity["metadata"]["namespace"]
            && object["metadata"]["name"] == identity["metadata"]["name"],
        "Kubernetes returned another event source resource"
    );
    Ok(json!({
        "apiVersion":object["apiVersion"],
        "kind":object["kind"],
        "metadata":{
            "name":object["metadata"]["name"],
            "namespace":object["metadata"]["namespace"],
            "generation":object["metadata"]["generation"],
            "labels":object["metadata"]["labels"],
        },
        "spec":object["spec"],
        "status":object["status"],
    }))
}

fn validate_status(status: &SourceStatus) -> Result<()> {
    ensure!(
        status.reason.len() <= 256
            && status.message.len() <= 4096
            && status
                .reason
                .chars()
                .chain(status.message.chars())
                .all(|character| !character.is_control() || character == '\n' || character == '\t'),
        "provider event source status contains invalid diagnostics"
    );
    ensure!(
        !status.ready || !status.terminal,
        "provider event source status cannot be ready and terminal"
    );
    Ok(())
}

fn identity(manifest: &Value) -> Result<String> {
    Ok(format!(
        "{}/{}/{}/{}",
        manifest["apiVersion"]
            .as_str()
            .context("manifest apiVersion is missing")?,
        manifest["kind"]
            .as_str()
            .context("manifest kind is missing")?,
        manifest["metadata"]["namespace"]
            .as_str()
            .context("manifest namespace is missing")?,
        manifest["metadata"]["name"]
            .as_str()
            .context("manifest name is missing")?,
    ))
}

fn record_name(namespace: &str, name: &str) -> String {
    let digest = Sha256::digest(format!("{namespace}\0{name}"));
    format!(
        "lampyr-source-{}",
        digest[..12]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}

fn read_record(kube: &Kube<'_>, namespace: &str, name: &str) -> Result<Option<SourceRecord>> {
    let Some(secret) = kube.optional(SYSTEM, "secret", &record_name(namespace, name))? else {
        return Ok(None);
    };
    let data = secret["data"]["source"]
        .as_str()
        .context("event source inventory is missing")?;
    let data = STANDARD.decode(data)?;
    ensure!(
        data.len() <= INVENTORY_LIMIT,
        "event source inventory exceeds 512 KiB"
    );
    let record: SourceRecord =
        serde_json::from_slice(&data).context("decode event source inventory")?;
    ensure!(record.schema == 1, "unsupported event source inventory");
    Ok(Some(record))
}

fn save_record(kube: &Kube<'_>, record: &SourceRecord) -> Result<()> {
    let data = serde_json::to_vec(record)?;
    ensure!(
        data.len() <= INVENTORY_LIMIT,
        "event source inventory exceeds 512 KiB"
    );
    kube.apply(&json!({
        "apiVersion":"v1",
        "kind":"Secret",
        "type":"Opaque",
        "metadata":{
            "name":record_name(&record.namespace, &record.name),
            "namespace":SYSTEM,
            "labels":{
                "app.kubernetes.io/managed-by":"lampyr",
                "lampyr.dev/installation-id":record.installation_id,
                "events.lampyr.dev/provider":record.provider,
                "events.lampyr.dev/source":record.name,
            }
        },
        "data":{"source":STANDARD.encode(data)}
    }))
}

fn delete_record(kube: &Kube<'_>, namespace: &str, name: &str) -> Result<()> {
    kube.delete_manifest(&json!({
        "apiVersion":"v1",
        "kind":"Secret",
        "metadata":{"name":record_name(namespace, name),"namespace":SYSTEM}
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source_kinds() -> BTreeSet<SourceKind> {
        BTreeSet::from([
            SourceKind {
                api_version: "eventing.knative.dev/v1".into(),
                kind: "Broker".into(),
            },
            SourceKind {
                api_version: "events.example.dev/v1".into(),
                kind: "ExampleSource".into(),
            },
        ])
    }

    #[test]
    fn source_inventory_names_are_stable_and_bounded() {
        assert_eq!(
            record_name("default", "example"),
            record_name("default", "example")
        );
        assert_ne!(
            record_name("other", "example"),
            record_name("default", "example")
        );
        assert!(record_name("default", &"x".repeat(63)).len() <= 63);
    }

    #[test]
    fn source_status_rejects_success_shaped_terminal_results() {
        assert!(
            validate_status(&SourceStatus {
                ready: true,
                reason: "Ready".into(),
                message: String::new(),
                terminal: true,
            })
            .is_err()
        );
    }

    #[test]
    fn expired_readiness_deadlines_fail_before_provider_status() {
        assert!(
            readiness_remaining(Instant::now(), "source", 1)
                .unwrap_err()
                .to_string()
                .contains("did not become ready within 1 seconds")
        );
    }

    #[test]
    fn teardown_rejects_live_azure_sources_without_cli_inventory() {
        let record = SourceRecord {
            schema: 1,
            provider: "azure".into(),
            installation_id: "a".repeat(32),
            namespace: "default".into(),
            name: "tracked".into(),
            service: "service".into(),
            plan: SourcePlan {
                shared_manifests: Vec::new(),
                manifests: Vec::new(),
                observations: Vec::new(),
                timeout_seconds: 30,
            },
        };
        let listing = json!({"items":[
            {"metadata":{"namespace":"default","name":"tracked"}},
            {"metadata":{"namespace":"other","name":"untracked"}}
        ]});
        assert_eq!(
            untracked_azure_sources(&[record], &listing).unwrap(),
            ["other/untracked"]
        );
    }

    #[test]
    fn generic_source_plans_are_bounded_and_claimed_without_provider_knowledge() {
        let shared = json!({
            "apiVersion":"eventing.knative.dev/v1",
            "kind":"Broker",
            "metadata":{"name":"default","namespace":"default"},
            "spec":{}
        });
        let owned = json!({
            "apiVersion":"events.example.dev/v1",
            "kind":"ExampleSource",
            "metadata":{"name":"source","namespace":"default"},
            "spec":{"sink":{"ref":{
                "apiVersion":"eventing.knative.dev/v1",
                "kind":"Broker",
                "name":"default"
            }}}
        });
        let mut plan = SourcePlan {
            shared_manifests: vec![shared.clone()],
            manifests: vec![owned.clone()],
            observations: vec![shared, owned],
            timeout_seconds: 30,
        };
        validate_plan(
            &mut plan,
            &source_kinds(),
            "example",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "default",
            "source",
            "service",
        )
        .unwrap();
        assert_eq!(
            plan.manifests[0]["metadata"]["labels"]["events.lampyr.dev/provider"],
            "example"
        );
        assert_eq!(
            plan.manifests[0]["metadata"]["labels"]["edge.lampyr.dev/service"],
            "service"
        );
        assert!(
            plan.shared_manifests[0]["metadata"]["labels"]
                .get("events.lampyr.dev/source")
                .is_none()
        );
    }

    #[test]
    fn observations_cannot_escape_the_source_manifest_plan() {
        let manifest = json!({
            "apiVersion":"events.example.dev/v1",
            "kind":"ExampleSource",
            "metadata":{"name":"source","namespace":"default"}
        });
        let mut plan = SourcePlan {
            shared_manifests: Vec::new(),
            manifests: vec![manifest],
            observations: vec![json!({
                "apiVersion":"v1",
                "kind":"Secret",
                "metadata":{"name":"unrelated","namespace":"default"}
            })],
            timeout_seconds: 30,
        };
        assert!(
            validate_plan(
                &mut plan,
                &source_kinds(),
                "example",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "default",
                "source",
                "service",
            )
            .is_err()
        );
    }

    #[test]
    fn source_plans_reject_unbounded_manifest_counts() {
        let manifest = json!({
            "apiVersion":"events.example.dev/v1",
            "kind":"ExampleSource",
            "metadata":{"name":"source","namespace":"default"}
        });
        let mut plan = SourcePlan {
            shared_manifests: Vec::new(),
            manifests: vec![manifest.clone(); MAX_MANIFESTS + 1],
            observations: vec![manifest],
            timeout_seconds: 30,
        };
        assert!(
            validate_plan(
                &mut plan,
                &source_kinds(),
                "example",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "default",
                "source",
                "service",
            )
            .is_err()
        );
    }

    #[test]
    fn source_plans_reject_undeclared_resource_kinds() {
        let mut plan = SourcePlan {
            shared_manifests: Vec::new(),
            manifests: vec![json!({
                "apiVersion":"events.example.dev/v1",
                "kind":"ExampleSource",
                "metadata":{"name":"source","namespace":"default"}
            })],
            observations: vec![json!({
                "apiVersion":"events.example.dev/v1",
                "kind":"ExampleSource",
                "metadata":{"name":"source","namespace":"default"}
            })],
            timeout_seconds: 30,
        };
        let declared = BTreeSet::from([SourceKind {
            api_version: "events.example.dev/v1".into(),
            kind: "AnotherSource".into(),
        }]);
        assert!(
            validate_plan(
                &mut plan,
                &declared,
                "example",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "default",
                "source",
                "service",
            )
            .unwrap_err()
            .to_string()
            .contains("undeclared kind")
        );
    }

    #[test]
    fn custom_source_kinds_must_come_from_the_approved_runtime_crds() {
        let runtime = json!({
            "before":[],
            "after":[{
                "apiVersion":"apiextensions.k8s.io/v1",
                "kind":"CustomResourceDefinition",
                "metadata":{"name":"examplesources.events.example.dev"},
                "spec":{
                    "group":"events.example.dev",
                    "scope":"Namespaced",
                    "names":{"kind":"ExampleSource"},
                    "versions":[{"name":"v1","served":true,"storage":true}]
                }
            }]
        });
        let approved = approved_custom_source_kinds(&runtime).unwrap();
        assert!(approved.contains(&SourceKind {
            api_version: "events.example.dev/v1".into(),
            kind: "ExampleSource".into(),
        }));
        assert!(!approved.contains(&SourceKind {
            api_version: "argoproj.io/v1alpha1".into(),
            kind: "Rollout".into(),
        }));
    }

    #[test]
    fn triggers_cannot_redirect_events_to_arbitrary_destinations() {
        let trigger = json!({
            "apiVersion":"eventing.knative.dev/v1",
            "kind":"Trigger",
            "metadata":{"name":"source","namespace":"default"},
            "spec":{
                "broker":"default",
                "subscriber":{"uri":"https://attacker.example/events"}
            }
        });
        assert!(
            validate_source_manifest_policy(&trigger, false, "default", "source", "service",)
                .is_err()
        );
        let mut missing_subscriber = trigger.clone();
        missing_subscriber["spec"]
            .as_object_mut()
            .unwrap()
            .remove("subscriber");
        let mut missing_ref = trigger.clone();
        missing_ref["spec"]["subscriber"] = json!({"uri":"/events/knative"});
        for malformed in [missing_subscriber, missing_ref] {
            assert!(
                validate_source_manifest_policy(&malformed, false, "default", "source", "service",)
                    .is_err()
            );
        }
    }

    #[test]
    fn custom_sources_can_target_only_the_selected_default_broker() {
        let source = json!({
            "apiVersion":"events.example.dev/v1",
            "kind":"ExampleSource",
            "metadata":{"name":"source","namespace":"default"},
            "spec":{"sink":{"ref":{
                "apiVersion":"eventing.knative.dev/v1",
                "kind":"Broker",
                "name":"default"
            }}}
        });
        validate_source_manifest_policy(&source, false, "default", "source", "service").unwrap();

        let mut external = source.clone();
        external["spec"]["sink"] = json!({"uri":"https://attacker.example/events"});
        let mut wrong_broker = source.clone();
        wrong_broker["spec"]["sink"]["ref"]["name"] = "other".into();
        let mut wrong_source = source.clone();
        wrong_source["metadata"]["name"] = "other".into();
        for malformed in [external, wrong_broker, wrong_source] {
            assert!(
                validate_source_manifest_policy(&malformed, false, "default", "source", "service",)
                    .is_err()
            );
        }
    }
}
