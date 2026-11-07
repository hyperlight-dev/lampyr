#!/usr/bin/env bash
set -euo pipefail

# Paid, destructive Azure integration test. It creates a disposable AKS
# cluster, Eventing platform, ACR, Front Door endpoint, Storage publisher,
# Lampyr Service, Broker, Trigger, and AzureEventGridSource, then deletes them.
#
# Grant Pods pull fiberd templates without credentials, which the Eventing
# ACR does not allow, so the suite also creates a Standard ACR that allows
# anonymous pulls for them, unless LAMPYR_E2E_TEMPLATE_REPOSITORY names a
# repository that does and that this host can push to.

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
# shellcheck source=scripts/fiberd-controller-inputs.sh
source "$repo_root/scripts/fiberd-controller-inputs.sh"
# AKS pulls the controller inputs from where they are published; built ones
# exist only on this host.
[[ -z $cluster_inputs ]] || {
  echo "the Azure suite needs published controller inputs, not LAMPYR_CLUSTER_INPUTS" >&2
  exit 1
}
subscription_id=${AZURE_SUBSCRIPTION_ID:?AZURE_SUBSCRIPTION_ID is required}
fiberd_image=${LAMPYR_E2E_FIBERD_IMAGE:?publish the serving controller image and set LAMPYR_E2E_FIBERD_IMAGE}
router_image=${LAMPYR_E2E_ROUTER_IMAGE:?publish the standalone consumer image and set LAMPYR_E2E_ROUTER_IMAGE}
node_setup_image=${LAMPYR_E2E_NODE_SETUP_IMAGE:?set LAMPYR_E2E_NODE_SETUP_IMAGE to a published digest}
controller_image=${LAMPYR_E2E_EVENTGRID_CONTROLLER_IMAGE:?set LAMPYR_E2E_EVENTGRID_CONTROLLER_IMAGE to a published digest}
adapter_image=${LAMPYR_E2E_EVENTGRID_ADAPTER_IMAGE:?set LAMPYR_E2E_EVENTGRID_ADAPTER_IMAGE to a published digest}
suffix=$(date -u +%m%d%H%M%S)
resource_group="lampyr-webhook-e2e-$suffix"
cluster="lampyr-webhook-$suffix"
location=${AZURE_LOCATION:-westus3}
storage="lye2e${suffix}"
template_repository=${LAMPYR_E2E_TEMPLATE_REPOSITORY:-}
# LAMPYR_E2E_TEMPLATE_REPOSITORY may spell https://, which install.json
# keeps and ly cluster install strips: the GrantPolicy spells TLS
# registries without a scheme, so its check compares the stripped form.
template_group=
service="hello-nodejs"
project=$(mktemp -d /tmp/lampyr-webhook-e2e.XXXXXX)
"$repo_root/build/ly" scaffold workspace "$project" >/dev/null
rm -r -- "$project/services/app"
printf 'apiVersion: lampyr.dev/v1alpha1\nkind: Workspace\ningress:\n  services: [%s]\n' \
  "$service" > "$project/lampyr.workspace.yaml"
context="aks-$cluster"
export LAMPYR_HOME="$project/data"
jq -n --arg subscription "$subscription_id" --arg group "$resource_group" \
  --arg location "$location" --arg fiberd "$fiberd_image" --arg router "$router_image" \
  --arg node_setup "$node_setup_image" \
  --arg controller "$controller_image" --arg adapter "$adapter_image" --arg storage "$storage" \
  '{subscriptionId:$subscription,resourceGroupName:$group,location:$location,
    fiberdImage:$fiberd,routerImage:$router,nodeSetupImage:$node_setup,
    eventGridControllerImage:$controller,eventGridAdapterImage:$adapter,
    eventSources:{"blob-events":{storageAccount:$storage,container:"events",
      eventTypes:["Microsoft.Storage.BlobCreated"]}}}' > "$project/provider.json"
if [[ -z ${LAMPYR_E2E_ARTIFACT:-} || -z ${LAMPYR_E2E_SOURCE_IMAGE:-} ]]; then
  host_vendor=$(awk -F: '/vendor_id/{gsub(/ /, "", $2); print $2; exit}' /proc/cpuinfo)
  if [[ $host_vendor != GenuineIntel ]]; then
    echo "The default AKS node is Intel but this host is $host_vendor." >&2
    echo "Run on an Intel /dev/kvm builder or set LAMPYR_E2E_ARTIFACT and LAMPYR_E2E_SOURCE_IMAGE." >&2
    exit 2
  fi
fi
# ly deploy seals and pushes the artifact's fiberd template with
# zygotectl, so a prebuilt LAMPYR_E2E_ARTIFACT, which must have been built
# with the helper's facts, needs the tools too.
prepare_fiberd_template_tools "$repo_root"

phase() {
  printf '\n[%s] %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*"
}

cleanup() {
  status=$?
  trap - EXIT
  phase "cleanup started (test status=$status)"
  if ((status != 0)); then
    phase "capturing failure diagnostics"
    kubectl --context "$context" get nodes,pods --all-namespaces -o wide 2>&1 || true
    kubectl --context "$context" get events --all-namespaces \
      --sort-by=.lastTimestamp 2>&1 | tail -100 || true
    kubectl --context "$context" -n fiberd-system describe deployment/fiberd-k8s-controller 2>&1 || true
    kubectl --context "$context" -n fiberd-system logs deployment/fiberd-k8s-controller \
      --tail=100 2>&1 || true
    kubectl --context "$context" -n default get capacitygrants.fiberd.io -o yaml 2>&1 || true
    kubectl --context "$context" -n kube-system logs \
      daemonset/lampyr-kvm-setup -c prepare-kvm --tail=100 2>&1 || true
    kubectl --context "$context" -n lampyr-system get all 2>&1 || true
    kubectl --context "$context" -n default get \
      azureeventgridsource/blob-events -o yaml 2>&1 || true
    kubectl --context "$context" -n lampyr-system get configmaps \
      --selector sources.lampyr.dev/adapter-config=true -o yaml 2>&1 || true
    kubectl --context "$context" -n lampyr-system logs \
      deployment/eventgrid-controller --tail=100 2>&1 || true
    kubectl --context "$context" -n lampyr-system logs \
      deployment/eventgrid-adapter --tail=100 2>&1 || true
    if [[ -n ${storage_id:-} ]]; then
      forwarding_subscription=$(kubectl --context "$context" -n default get \
        azureeventgridsource/blob-events \
        --output 'jsonpath={.status.azureResources.forwardingSubscriptionName}' \
        2>/dev/null || true)
      if [[ -n $forwarding_subscription ]]; then
        az monitor metrics list \
          --resource "$storage_id/providers/Microsoft.EventGrid/eventSubscriptions/$forwarding_subscription" \
          --metric DeliverySuccessCount DeliveryAttemptFailCount \
          --interval PT1M \
          --aggregation Total \
          --offset 30m \
          -o table 2>&1 || true
      fi
    fi
    if [[ ${KEEP_FAILED_RESOURCES:-false} == true ]]; then
      phase "retaining failed resources for inspection"
      printf 'resource group: %s\ncluster: %s\ncontext: %s\n' \
        "$resource_group" "$cluster" "$context"
      if [[ -n $template_group ]]; then
        printf 'template resource group: %s\n' "$template_group"
      fi
      exit "$status"
    fi
  fi
  timeout 40m build/ly cluster uninstall \
    --context "$context" \
    --provider azure \
    --provider-config "$project/provider.json" \
    --name "$cluster" \
    --delete-cluster >/dev/null 2>&1 || status=1
  phase "AKS uninstall request completed"
  az group delete --subscription "$subscription_id" \
    --name "$resource_group" --yes --no-wait >/dev/null 2>&1 || status=1
  cleanup_groups=("$resource_group")
  if [[ -n $template_group ]]; then
    az group delete --subscription "$subscription_id" \
      --name "$template_group" --yes --no-wait >/dev/null 2>&1 || status=1
    cleanup_groups+=("$template_group")
  fi
  for group in $(az group list --subscription "$subscription_id" \
    --query "[?tags.\"lampyr-cluster\"=='$cluster'].name" -o tsv); do
    az group delete --subscription "$subscription_id" \
      --name "$group" --yes --no-wait >/dev/null 2>&1 || status=1
    cleanup_groups+=("$group")
  done
  for group in "${cleanup_groups[@]}"; do
    for attempt in $(seq 1 80); do
      exists=$(az group exists \
        --subscription "$subscription_id" \
        --name "$group" 2>/dev/null || echo true)
      [[ "$exists" == "false" ]] && break
      sleep 15
    done
    if [[ $(az group exists \
      --subscription "$subscription_id" \
      --name "$group" 2>/dev/null || echo true) != "false" ]]; then
      printf 'Azure resource group %s was not deleted\n' "$group" >&2
      status=1
    fi
  done
  rm -rf "$project"
  phase "cleanup finished (exit status=$status)"
  exit "$status"
}
trap cleanup EXIT

phase "building ly and Event Grid components"
just build-ly azure::build build-eventgrid
if [[ -z $template_repository ]]; then
  phase "creating an anonymously pullable ACR for fiberd templates"
  template_group="$resource_group-templates"
  template_registry="lye2etpl${suffix}"
  az group create --subscription "$subscription_id" \
    --name "$template_group" --location "$location" >/dev/null
  # Only the Standard and Premium tiers allow anonymous pulls.
  az acr create --subscription "$subscription_id" \
    --resource-group "$template_group" --name "$template_registry" \
    --sku Standard --admin-enabled false --anonymous-pull-enabled true >/dev/null
  az acr login --subscription "$subscription_id" --name "$template_registry"
  template_repository="$template_registry.azurecr.io/lampyr/fiberd-templates"
fi
# Install also takes fiberd-k8s-controller's inputs, which ly keeps from
# the provider. They go in install.json alone, since uninstall needs none.
# AKS makes no registry grant Pods can pull templates from, so the install
# gives zygoteRegistries, and ly deploy pushes templates to the first.
jq --arg controllerManifest "$controller_manifest" --arg grantImage "$grant_image" \
  --arg helperImage "$helper_image" --arg templates "$template_repository" \
  '. + {controllerManifest:$controllerManifest,grantImage:$grantImage,
    helperImage:$helperImage,zygoteRegistries:[$templates]}' \
  "$project/provider.json" > "$project/install.json"
phase "installing AKS cluster and Eventing platform"
timeout 75m build/ly cluster install \
  --provider azure \
  --approve-provider-manifests \
  --eventing \
  --provider-config "$project/install.json" \
  --workspace "$project" \
  --name "$cluster"

inventory=$(kubectl --context "$context" -n kube-system get \
  configmap/lampyr-install \
  --output 'jsonpath={.data.eventing\.azure\.resources}')
registry_login_server=$(jq -r .registryLoginServer <<<"$inventory")
registry_name=${registry_login_server%.azurecr.io}
policy=$(kubectl --context "$context" get grantpolicies.fiberd.io default \
  -o jsonpath='{.spec.registries}')
[[ $policy == "[\"${template_repository#https://}\"]" ]] || {
  printf 'GrantPolicy registries are %s\n' "$policy" >&2
  exit 1
}
phase "authenticating to Lampyr ACR"
az acr login --subscription "$subscription_id" --name "$registry_name"
phase "checking cluster and Eventing health"
timeout 5m build/ly cluster health \
  --context "$context" \
  --provider azure \
  --name "$cluster"


phase "creating Blob Storage publisher"
az storage account create \
  --subscription "$subscription_id" \
  --resource-group "$resource_group" \
  --name "$storage" \
  --location "$location" \
  --sku Standard_LRS \
  --kind StorageV2 \
  --allow-shared-key-access false \
  --allow-blob-public-access false >/dev/null
storage_id=$(az storage account show \
  --subscription "$subscription_id" \
  --resource-group "$resource_group" \
  --name "$storage" \
  --query id -o tsv)
arm_token=$(az account get-access-token \
  --subscription "$subscription_id" \
  --resource https://management.azure.com \
  --query accessToken -o tsv)
user_oid=$(python3 -c '
import base64,json,sys
p=sys.stdin.read().strip().split(".")[1]; p += "="*(-len(p)%4)
print(json.loads(base64.urlsafe_b64decode(p))["oid"])
' <<<"$arm_token")
unset arm_token
az role assignment create \
  --subscription "$subscription_id" \
  --assignee-object-id "$user_oid" \
  --assignee-principal-type User \
  --role "Storage Blob Data Contributor" \
  --scope "$storage_id" >/dev/null
for _ in $(seq 1 24); do
  az storage container create \
    --account-name "$storage" \
    --auth-mode login \
    --name events >/dev/null && break
  sleep 10
done

phase "building and pushing Lampyr Service"
service_dir="$project/services/$service"
cp -a "$repo_root/docs/examples/$service" "$service_dir"
rm -rf "$service_dir/.lampyr"
image="$registry_login_server/artifacts/$service:$suffix"
artifact_args=()
if [[ -n ${LAMPYR_E2E_ARTIFACT:-} && -n ${LAMPYR_E2E_SOURCE_IMAGE:-} ]]; then
  phase "publishing the prebuilt compatibility-class artifact to ACR"
  if ! docker image inspect "$LAMPYR_E2E_SOURCE_IMAGE" >/dev/null 2>&1; then
    docker pull "$LAMPYR_E2E_SOURCE_IMAGE"
  fi
  docker tag "$LAMPYR_E2E_SOURCE_IMAGE" "$image"
  docker push "$image"
  artifact_args=(--artifact "$LAMPYR_E2E_ARTIFACT")
else
  sed -i 's/^    cpu: 250m$/    cpu: 10m/' "$service_dir/lampyr.yaml"
  build/ly build "$service_dir"
  build/ly push --image "$image" "$service_dir"
fi
phase "deploying Lampyr Service to AKS"
build/ly deploy --context "$context" --image "$image" \
  "${artifact_args[@]}" "$service_dir"

phase "creating Azure Blob event source"
build/ly event source create \
  --provider azure \
  --name blob-events \
  --context "$context" \
  "$service_dir"

# The Revision's CapacityGrant shares its name. fiberd-k8s-controller
# grants each grant Pod under the Pod's own UID, which fiberd stamps on its
# audit records, so the Pod is found once and its Fiber starts counted.
revision=$(kubectl --context "$context" -n default get \
  "service.serving.knative.dev/$service" -o jsonpath='{.status.latestReadyRevisionName}')
home=$(python3 -c '
import sys
sys.path.insert(0, sys.argv[1])
import fiberd_grants
print(*fiberd_grants.home_pod(sys.argv[2], fiberd_grants.grant(sys.argv[2], sys.argv[3])))
' "$repo_root/scripts" "$context" "$revision")
read -r home_pod home_uid <<<"$home"
fibers_started() {
  python3 -c '
import sys
sys.path.insert(0, sys.argv[1])
import fiberd_grants
home = fiberd_grants.Home(sys.argv[3], sys.argv[4])
print(fiberd_grants.fibers_started(fiberd_grants.audit_records(sys.argv[2], home)))
' "$repo_root/scripts" "$context" "$home_pod" "$home_uid"
}
before=$(fibers_started)
printf 'Lampyr direct Event Grid webhook end-to-end proof\n' >"$project/blob.txt"

phase "publishing Blob events and waiting up to 5 minutes for a Fiber start"
published=0
for attempt in $(seq 1 60); do
  if (((attempt - 1) % 6 == 0)); then
    published=$((published + 1))
    az storage blob upload \
      --account-name "$storage" \
      --auth-mode login \
      --container-name events \
      --name "proof-$published.txt" \
      --file "$project/blob.txt" \
      --overwrite >/dev/null
    phase "published Blob event $published"
  fi
  sleep 5
  after=$(fibers_started)
  if ((after > before)); then
    phase "waiting for the Blob payload in Lampyr Service handler logs"
    for log_attempt in $(seq 1 12); do
      handler_logs=$(build/ly logs \
        --context "$context" \
        --tail 20 \
        "$service_dir")
      if grep -q '"kind":"handler"' <<<"$handler_logs" &&
        grep -q "proof-$published.txt" <<<"$handler_logs"; then
        printf '%s\n' "$handler_logs"
        echo "AKS direct Event Grid webhook Blob-to-Lampyr delivery succeeded"
        exit 0
      fi
      sleep 5
    done
    echo "A Fiber started but the Blob payload did not appear in handler logs" >&2
    exit 1
  fi
  if ((attempt % 6 == 0)); then
    phase "delivery still pending ($((attempt * 5))/300 seconds; $published Blob events published)"
  fi
done
kubectl --context "$context" -n knative-eventing logs \
  deployment/mt-broker-filter --since=10m --tail=100 2>&1 || true
kubectl --context "$context" -n knative-serving logs \
  deployment/activator --since=10m --tail=100 2>&1 || true
kubectl --context "$context" -n default logs \
  "pod/$home_pod" --container fiberd --since=10m --tail=100 2>&1 || true
echo "Blob event did not reach Lampyr Service" >&2
exit 1
