---
title: Azure Event Grid source
description: Deliver Azure Blob events to a Lampyr Service on AKS.
---

# Azure Event Grid source

`AzureEventGridSource` is a Knative-compatible source. It preserves CloudEvents
and reports success to its provider only after the configured sink accepts the
event.

## Azure walkthrough: Blob Storage to Node.js

This walkthrough creates AKS with Eventing, scaffolds and deploys a Node.js
Lampyr Service, and routes Blob-created events through Event Grid.

Run it from the Lampyr repository root with Azure CLI, `kubectl`, `jq`,
Python 3, and Docker available. Hyperlight snapshots are CPU-vendor-specific,
so the build step below creates the snapshot on the Intel AKS node rather than
assuming the local workstation has a matching CPU.

### 1. Install AKS with Eventing

```bash
just build-ly azure::build azure::build-eventgrid

STORAGE="lyevt$(date -u +%m%d%H%M%S)"

cat > azure.yaml <<EOF
subscriptionId: 11111111-1111-1111-1111-111111111111
fiberdImage: ghcr.io/hyperlight-dev/lampyr/fiberd:preview@sha256:FIBERD_DIGEST
routerImage: ghcr.io/hyperlight-dev/lampyr/fiber-router:preview@sha256:ROUTER_DIGEST
nodeSetupImage: ghcr.io/hyperlight-dev/lampyr/azure-node-setup:preview@sha256:NODE_SETUP_DIGEST
eventGridControllerImage: ghcr.io/hyperlight-dev/lampyr/eventgrid-controller:preview@sha256:CONTROLLER_DIGEST
eventGridAdapterImage: ghcr.io/hyperlight-dev/lampyr/eventgrid-adapter:preview@sha256:ADAPTER_DIGEST
eventSources:
  blob-created:
    storageAccount: $STORAGE
    container: events
    eventTypes:
      - Microsoft.Storage.BlobCreated
EOF
```

Use your subscription ID; the guest prompts for its own device-code login.
This example uses cluster and resource group `lampyr`, with context
`aks-lampyr`. See the [AKS cluster guide](cluster-aks.md) for building and
publishing the platform images, prerequisites, authentication, subscription
selection, and installation overrides. If you choose another cluster name, use
its exact context throughout this walkthrough.

Replace the digest placeholders with the published preview image SHA-256
digests. The guest never builds or pushes images.

For the preview images, use
`just preview-mirror-images destination=REGISTRY/lampyr` as described in the
[AKS cluster guide](cluster-aks.md). The recipe automatically resolves
`preview-release` to the canonical internal
`preview-<full-release-commit>` tags for all eight images; do not substitute the
moving `:preview` convenience aliases. Then inspect the generated
image-settings fragment:

```bash
cat build/preview-azure.yaml
```

In the `azure.yaml` created at the start of this section, replace only
`fiberdImage`, `routerImage`, `nodeSetupImage`, `eventGridControllerImage`, and
`eventGridAdapterImage` with the generated values, and add its
`controllerManifest`, `grantImage` and `helperImage`. Add `zygoteRegistries`,
naming a repository that allows anonymous pulls, for fiberd templates: grant
Pods pull them without credentials, which the Eventing ACR does not allow, so
use another, such as a Standard ACR with anonymous pull enabled (see
[Fiberd templates](cli/cluster.md#fiberd-templates)), and log in to it before
`ly deploy`, which pushes the templates there. Leave `subscriptionId`,
`eventSources`, and the other settings unchanged. The generated node setup and
Event Grid image references are digest-pinned and named explicitly for the
controller and adapter. The AKS nodes must have pull access to the mirror.
Lampyr does not create registry credentials or image pull secrets.

After `azure.yaml` contains both the customer settings and the published image
settings, install:

```bash
build/ly cluster install --provider azure --eventing \
  --provider-config azure.yaml \
  --approve-provider-manifests
```

The command creates AKS, Knative Serving and Eventing, Front Door, the
restricted webhook origin, ACR, Workload Identities, and Event Grid roles.
It also scaffolds `event-sources.json` into the customer-owned Azure workspace;
edit that file when source definitions change.
Front Door propagation can take more than 30 minutes; source creation waits
for end-to-end validation.

### Understand the current state

Check the local workstation:

```bash
build/ly doctor
```

[`doctor`](cli/doctor.md) reports the Lampyr runtime payload it would use and local
prerequisites such as Docker, `kubectl`, glibc and `/dev/kvm`, and exits
non-zero when one is missing. It does not inspect AKS.

Check the Lampyr, Knative, Eventing, ingress, node-runtime, and Azure platform
resources:

```bash
build/ly cluster health --context aks-lampyr
```

Inspect the Kubernetes state directly:

```bash
kubectl --context aks-lampyr get nodes -o wide
kubectl --context aks-lampyr get pods --all-namespaces -o wide
kubectl --context aks-lampyr get events --all-namespaces \
  --sort-by=.lastTimestamp | tail -100
```

Inspect the installed Lampyr metadata and Eventing workloads:

```bash
kubectl --context aks-lampyr -n kube-system get configmap/lampyr-install -o yaml
kubectl --context aks-lampyr -n knative-eventing get pods
kubectl --context aks-lampyr -n lampyr-system get pods,services
```

After creating the Broker, Trigger, and source, inspect their readiness and
condition messages:

```bash
kubectl --context aks-lampyr -n default get \
  broker,trigger,azureeventgridsource
kubectl --context aks-lampyr -n default describe \
  azureeventgridsource/blob-created
```

After deploying the example Lampyr Service, inspect its Revision, URL, Fiber
state, and recent activation audit records:

```bash
build/ly status --context aks-lampyr services/blob-handler
build/ly logs --context aks-lampyr --tail 20 services/blob-handler
```

### 2. Scaffold and deploy a Node.js Lampyr Service

```bash
build/ly scaffold service blob-handler --runtime nodejs
build/ly build \
  --context aks-lampyr \
  services/blob-handler
```

The generated project enables the event protocol and exposes
`/events/knative`, which acknowledges Broker deliveries with `204 No Content`.

The registry name is deterministic but includes a subscription-specific hash
to remain globally unique. Read it once from Lampyr's cluster metadata,
authenticate, and publish the service:

```bash
REGISTRY=$(
  kubectl --context aks-lampyr -n kube-system get configmap/lampyr-install \
    --output 'jsonpath={.data.eventing\.azure\.resources}' |
    jq -r .registryLoginServer
)

az acr login --name "${REGISTRY%.azurecr.io}"

build/ly deploy \
  --context aks-lampyr \
  --image "$REGISTRY/services/blob-handler:demo" \
  services/blob-handler
```

### 3. Create a Blob Storage publisher

Storage account names are globally unique, so the timestamp is part of this
example:

```bash
az storage account create \
  --resource-group lampyr \
  --name "$STORAGE" \
  --location westus3 \
  --sku Standard_LRS \
  --kind StorageV2 \
  --allow-shared-key-access false \
  --allow-blob-public-access false

STORAGE_ID=$(
  az storage account show \
    --resource-group lampyr \
    --name "$STORAGE" \
    --query id \
    --output tsv
)

USER_OBJECT_ID=$(
  az account get-access-token \
    --resource https://management.azure.com/ \
    --query accessToken \
    --output tsv |
    python3 -c 'import base64,json,sys; token=sys.stdin.read().strip(); payload=token.split(".")[1]; payload += "=" * (-len(payload) % 4); print(json.loads(base64.urlsafe_b64decode(payload))["oid"])'
)

az role assignment create \
  --assignee-object-id "$USER_OBJECT_ID" \
  --assignee-principal-type User \
  --role "Storage Blob Data Contributor" \
  --scope "$STORAGE_ID"
```

The data-plane role can take a minute to propagate. Create the container with a
bounded retry:

```bash
for attempt in $(seq 1 24); do
  if az storage container create \
    --account-name "$STORAGE" \
    --auth-mode login \
    --name events; then
    break
  fi
  sleep 10
done

az storage container show \
  --account-name "$STORAGE" \
  --auth-mode login \
  --name events \
  --output none
```

### 4. Connect Event Grid to the service

Create an Azure Blob event source for the Lampyr Service:

```bash
build/ly event source create \
  --provider azure \
  --name blob-created \
  --context aks-lampyr \
  services/blob-handler
```

Lampyr creates and owns the required Eventing routing resources, waits for
Azure subscription validation, and reports source condition progress. Inspect
the resulting source later with:

```bash
build/ly event source status \
  --provider azure \
  --name blob-created \
  --context aks-lampyr \
  services/blob-handler
```

### 5. Publish and verify an event

Allow a short interval after source readiness for Event Grid's delivery plane,
then upload a Blob:

```bash
printf 'hello from Azure Event Grid\n' >/tmp/lampyr-event.txt
sleep 30

az storage blob upload \
  --account-name "$STORAGE" \
  --auth-mode login \
  --container-name events \
  --name "proof-$(date -u +%s).txt" \
  --file /tmp/lampyr-event.txt
```

Verify the handler received the Blob event:

```bash
build/ly status --context aks-lampyr services/blob-handler
build/ly logs --context aks-lampyr --tail 20 services/blob-handler
build/ly audit --context aks-lampyr --tail 20 services/blob-handler
```

`ly logs` shows the CloudEvent identity and parsed Blob-created payload;
`ly audit` shows the corresponding Fiber `create` and `release` records. If the
subscription has only just become Ready and no handler record appears within a
minute, upload another Blob with a new name. Event Grid does not replay an
event published before its data plane is active.

### 6. Clean up

```bash
build/ly event source delete \
  --provider azure \
  --name blob-created \
  --context aks-lampyr \
  services/blob-handler

build/ly delete --context aks-lampyr services/blob-handler

build/ly cluster uninstall \
  --context aks-lampyr \
  --delete-cluster

az group delete \
  --name lampyr \
  --yes \
  --no-wait
```

The Eventing resource group is deleted asynchronously because Front Door
cleanup can take several minutes.

## Run on AKS

See [Azure walkthrough: Blob Storage to Node.js](#azure-walkthrough-blob-storage-to-nodejs)
for the complete install, scaffold, deploy, source, publish, verify, and cleanup
flow.

## Delivery behavior

- Existing CloudEvent attributes, extensions, `data`, and `data_base64` are
  preserved.
- Only explicitly configured extension overrides are changed.
- On AKS, sink failures return a server error so Event Grid retries.
- Invalid CloudEvents are rejected under the poison-event policy.
- Consumers should use `(source, id)` as their idempotency key.

The default Broker uses InMemoryChannel and is non-durable preview
infrastructure.
