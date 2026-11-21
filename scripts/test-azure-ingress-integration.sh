#!/usr/bin/env bash
set -euo pipefail

# Paid Azure integration test. It creates AKS ingress resources in an existing
# public DNS zone. It never creates or deletes that zone.

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
declare cluster_inputs controller_manifest grant_image helper_image
# shellcheck disable=SC1091
# shellcheck source=scripts/fiberd-controller-inputs.sh
source "$repo_root/scripts/fiberd-controller-inputs.sh"
[[ -z $cluster_inputs ]] || {
  echo "the Azure suite needs published controller inputs" >&2
  exit 1
}

subscription_id=${AZURE_SUBSCRIPTION_ID:?AZURE_SUBSCRIPTION_ID is required}
zone_id=${AZURE_DNS_ZONE_RESOURCE_ID:?AZURE_DNS_ZONE_RESOURCE_ID is required}
domain=${LAMPYR_E2E_DOMAIN:?LAMPYR_E2E_DOMAIN is required}
changed_domain=${LAMPYR_E2E_CHANGED_DOMAIN:?LAMPYR_E2E_CHANGED_DOMAIN is required}
email=${LAMPYR_E2E_ACME_EMAIL:?LAMPYR_E2E_ACME_EMAIL is required}
template_repository=${LAMPYR_E2E_TEMPLATE_REPOSITORY:?LAMPYR_E2E_TEMPLATE_REPOSITORY is required}
fiberd_image=${LAMPYR_E2E_FIBERD_IMAGE:?LAMPYR_E2E_FIBERD_IMAGE is required}
router_image=${LAMPYR_E2E_ROUTER_IMAGE:?LAMPYR_E2E_ROUTER_IMAGE is required}
node_setup_image=${LAMPYR_E2E_NODE_SETUP_IMAGE:?LAMPYR_E2E_NODE_SETUP_IMAGE is required}
location=${AZURE_LOCATION:-westus3}
suffix=$(date -u +%m%d%H%M%S)
cluster="lampyr-ingress-$suffix"
resource_group="$cluster"
context="aks-$cluster"
project="$repo_root/.lampyr-azure-ingress-e2e.$suffix"
mkdir "$project"
export LAMPYR_HOME="$project/data"
uninstalled=false
install_attempted=false
ingress_inventory=

zone_name=${zone_id##*/}
read -r zone_subscription zone_group < <(
  python3 - "$zone_id" <<'PY'
import re
import sys
match = re.fullmatch(
    r"/subscriptions/([^/]+)/resourceGroups/([^/]+)/providers/"
    r"Microsoft\.Network/dnsZones/([^/]+)",
    sys.argv[1],
    re.IGNORECASE,
)
if not match:
    raise SystemExit("AZURE_DNS_ZONE_RESOURCE_ID is invalid")
print(match.group(1), match.group(2))
PY
)
record_name() {
  python3 - "$1" "$zone_name" <<'PY'
import sys
domain, zone = sys.argv[1:]
if domain == zone:
    print("*")
elif domain.endswith("." + zone):
    print("*." + domain[:-(len(zone) + 1)])
else:
    raise SystemExit(f"{domain} is outside {zone}")
PY
}
old_record=$(record_name "$domain")
new_record=$(record_name "$changed_domain")
default_record() {
  if [[ $1 == "*" ]]; then
    echo "*.default"
  else
    echo "*.default.${1#*.}"
  fi
}

phase() {
  printf '\n[%s] %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*"
}

cleanup() {
  status=$?
  trap - EXIT
  cleanup_succeeded=true
  remaining_resources=()
  cleanup_failed() {
    cleanup_succeeded=false
    status=1
  }
  installation_record="$LAMPYR_HOME/installations/azure/$cluster/host/installation.json"
  installation_id=
  if [[ -f $installation_record ]]; then
    installation_id=$(jq -er .id "$installation_record") || {
      cleanup_failed
      remaining_resources+=("installation state $installation_record")
      installation_id=
    }
  fi
  if [[ $uninstalled == false && -f $project/provider.json ]]; then
    if [[ -z $ingress_inventory ]]; then
      ingress_inventory=$(kubectl --context "$context" -n kube-system \
        get configmap lampyr-install \
        -o 'jsonpath={.data.ingress\.azure\.resources}' 2>/dev/null) ||
        ingress_inventory=
    fi
    if "$repo_root/build/ly" cluster uninstall \
      --provider azure \
      --provider-config "$project/provider.json" \
      --name "$cluster" \
      --workspace "$project" \
      --approve-provider-manifests >/dev/null; then
      uninstalled=true
    else
      cleanup_failed
      remaining_resources+=("Lampyr installation $cluster")
    fi
  fi
  if [[ -n $installation_id ]]; then
    if records=$(az network dns record-set a list \
        --subscription "$zone_subscription" \
        --resource-group "$zone_group" \
        --zone-name "$zone_name" -o json); then
      owned_records=$(jq -r --arg owner "$installation_id" \
        '.[] | select(.metadata["lampyr-installation-id"] == $owner) | [.name,.etag] | @tsv' \
        <<<"$records") || {
        cleanup_failed
        remaining_resources+=("owned DNS records in zone $zone_name")
        owned_records=
      }
      while IFS=$'\t' read -r record etag; do
        [[ -z $record ]] && continue
        if [[ -z $etag ]]; then
          cleanup_failed
          remaining_resources+=("DNS record $record.$zone_name")
          continue
        fi
        if ! az network dns record-set a delete \
          --subscription "$zone_subscription" \
          --resource-group "$zone_group" \
          --zone-name "$zone_name" \
          --name "$record" --if-match "$etag" --yes >/dev/null; then
          cleanup_failed
          remaining_resources+=("DNS record $record.$zone_name")
        fi
      done <<<"$owned_records"
    else
      cleanup_failed
      remaining_resources+=("owned DNS records in zone $zone_name")
    fi
    ingress_group="$cluster-ingress-${installation_id:0:10}"
    if ingress_exists=$(az group exists --subscription "$subscription_id" \
      --name "$ingress_group"); then
      if [[ $ingress_exists == true ]]; then
        ingress_cleanup_succeeded=true
        if ingress_group_json=$(az group show --subscription "$subscription_id" \
          --name "$ingress_group" -o json); then
          ingress_owner=$(jq -r '(.tags // {})["lampyr-installation-id"] // ""' \
            <<<"$ingress_group_json") || ingress_owner=
          if [[ $ingress_owner != "$installation_id" ]]; then
            cleanup_failed
            ingress_cleanup_succeeded=false
            remaining_resources+=(
              "resource group $ingress_group is not owned by installation $installation_id"
            )
          fi
        else
          cleanup_failed
          ingress_cleanup_succeeded=false
          remaining_resources+=("resource group $ingress_group ownership")
        fi
        expected_role=
        expected_principal=
        if [[ $ingress_cleanup_succeeded == true ]]; then
          expected_role=$(jq -er .dnsRoleAssignmentId <<<"$ingress_inventory") || true
          expected_principal=$(jq -er .dnsIdentityPrincipalId <<<"$ingress_inventory") || true
          if [[ -z $expected_role || -z $expected_principal ]]; then
            cleanup_failed
            ingress_cleanup_succeeded=false
            remaining_resources+=("recorded Azure DNS role assignment")
          fi
        fi
        if [[ $ingress_cleanup_succeeded == true ]]; then
          if principals=$(az identity list --subscription "$subscription_id" \
            --resource-group "$ingress_group" \
            --query "[?tags.\"lampyr-installation-id\"=='$installation_id'].principalId" \
            -o tsv); then
            if ! grep -Fqx "$expected_principal" <<<"$principals"; then
              cleanup_failed
              ingress_cleanup_succeeded=false
              remaining_resources+=("managed identity $expected_principal")
            fi
          else
            cleanup_failed
            ingress_cleanup_succeeded=false
            remaining_resources+=("managed identities in resource group $ingress_group")
          fi
        fi
        if [[ $ingress_cleanup_succeeded == true ]]; then
          if assignments=$(az role assignment list --scope "$zone_id" \
            --assignee-object-id "$expected_principal" \
            --fill-principal-name false -o json); then
            matching=$(jq -c --arg id "$expected_role" \
              '[.[] | select((.id | ascii_downcase) == ($id | ascii_downcase))]' \
              <<<"$assignments") || matching=[]
            match_count=$(jq length <<<"$matching")
            if [[ $match_count == 0 ]]; then
              :
            elif [[ $match_count != 1 ]]; then
              cleanup_failed
              ingress_cleanup_succeeded=false
              remaining_resources+=("role assignment $expected_role")
            else
              assignment_scope=$(jq -r '.[0].scope // ""' <<<"$matching")
              assignment_principal=$(jq -r '.[0].principalId // ""' <<<"$matching")
              assignment_role=$(jq -r '.[0].roleDefinitionId // ""' <<<"$matching")
              role_prefix="$zone_id/providers/Microsoft.Authorization/roleAssignments/"
              role_suffix=${expected_role:${#role_prefix}}
              if [[ ${assignment_scope,,} != "${zone_id,,}" ||
                ${assignment_principal,,} != "${expected_principal,,}" ||
                ${assignment_role,,} != */befefa01-2a29-4197-83a8-272ff33ce314 ||
                ${expected_role,,} != "${role_prefix,,}"* ||
                ! $role_suffix =~ ^[0-9A-Fa-f]{8}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{12}$ ]]; then
                cleanup_failed
                ingress_cleanup_succeeded=false
                remaining_resources+=("role assignment $expected_role ownership")
              elif ! az role assignment delete --ids "$expected_role" >/dev/null; then
                cleanup_failed
                ingress_cleanup_succeeded=false
                remaining_resources+=("role assignment $expected_role")
              fi
            fi
          else
            cleanup_failed
            ingress_cleanup_succeeded=false
            remaining_resources+=("role assignment $expected_role")
          fi
        fi
        if [[ $ingress_cleanup_succeeded == true ]]; then
          if az group delete --subscription "$subscription_id" --name "$ingress_group" \
            --yes --no-wait >/dev/null; then
            if ! az group wait --deleted --subscription "$subscription_id" \
              --resource-group "$ingress_group" --timeout 1800; then
              cleanup_failed
              remaining_resources+=("resource group $ingress_group")
            fi
          else
            cleanup_failed
            remaining_resources+=("resource group $ingress_group")
          fi
        else
          remaining_resources+=("resource group $ingress_group")
        fi
      fi
    else
      cleanup_failed
      remaining_resources+=("resource group $ingress_group")
    fi
  elif [[ $uninstalled == false && $install_attempted == true ]]; then
    cleanup_failed
    if [[ ! -f $installation_record ]]; then
      remaining_resources+=("installation state $installation_record")
    fi
    remaining_resources+=("resource group matching $cluster-ingress-*")
    remaining_resources+=("owned DNS records in zone $zone_name (installation id unavailable)")
    remaining_resources+=(
      "resource group $resource_group ownership (installation id unavailable)"
    )
  fi
  if [[ $install_attempted == true && -n $installation_id ]]; then
    if cluster_group_exists=$(az group exists --subscription "$subscription_id" \
      --name "$resource_group"); then
      if [[ $cluster_group_exists == true ]]; then
        if cluster_group_json=$(az group show --subscription "$subscription_id" \
          --name "$resource_group" -o json); then
          cluster_owner=$(jq -r '(.tags // {})["lampyr-installation-id"] // ""' \
            <<<"$cluster_group_json") || cluster_owner=
        else
          cluster_owner=
        fi
        if [[ $cluster_owner != "$installation_id" ]]; then
          cleanup_failed
          remaining_resources+=(
            "resource group $resource_group is not owned by installation $installation_id"
          )
        elif az group delete --subscription "$subscription_id" --name "$resource_group" \
          --yes --no-wait >/dev/null; then
          if ! az group wait --deleted --subscription "$subscription_id" \
            --resource-group "$resource_group" --timeout 1800; then
            cleanup_failed
            remaining_resources+=("resource group $resource_group")
          fi
        else
          cleanup_failed
          remaining_resources+=("resource group $resource_group")
        fi
      fi
    else
      cleanup_failed
      remaining_resources+=("resource group $resource_group")
    fi
  fi
  if [[ $cleanup_succeeded == true ]]; then
    rm -rf "$project"
  else
    echo "retained Azure ingress test workspace: $project" >&2
    if ((${#remaining_resources[@]})); then
      printf 'cleanup still requires: %s\n' "${remaining_resources[@]}" >&2
    fi
  fi
  exit "$status"
}
trap cleanup EXIT

phase "building Azure provider and CLI"
just --justfile "$repo_root/Justfile" build-ly azure::build
"$repo_root/build/ly" scaffold workspace "$project" >/dev/null

jq -n \
  --arg subscription "$subscription_id" \
  --arg group "$resource_group" \
  --arg location "$location" \
  --arg domain "$domain" \
  --arg zone "$zone_id" \
  --arg email "$email" \
  --arg fiberd "$fiberd_image" \
  --arg router "$router_image" \
  --arg node_setup "$node_setup_image" \
  --arg controller_manifest "$controller_manifest" \
  --arg grant_image "$grant_image" \
  --arg helper_image "$helper_image" \
  --arg templates "$template_repository" \
  '{
    subscriptionId:$subscription,
    resourceGroupName:$group,
    location:$location,
    domain:$domain,
    ingress:{
      azureDnsZoneResourceId:$zone,
      tls:{mode:"acme",email:$email,issuer:"letsencrypt-prod"}
    },
    fiberdImage:$fiberd,
    routerImage:$router,
    nodeSetupImage:$node_setup,
    controllerManifest:$controller_manifest,
    grantImage:$grant_image,
    helperImage:$helper_image,
    zygoteRegistries:[$templates]
  }' >"$project/provider.json"

phase "installing production ingress"
install_attempted=true
timeout 75m "$repo_root/build/ly" cluster install \
  --provider azure \
  --provider-config "$project/provider.json" \
  --workspace "$project" \
  --name "$cluster" \
  --approve-provider-manifests

certificate_serial() {
  kubectl --context "$context" -n kourier-system get secret lampyr-kourier-tls \
    -o jsonpath='{.data.tls\.crt}' |
    base64 --decode |
    openssl x509 -noout -serial
}

endpoint_serial() {
  openssl s_client \
    -connect "lampyr-readiness.default.$domain:443" \
    -servername "lampyr-readiness.default.$domain" </dev/null 2>/dev/null |
    openssl x509 -noout -serial
}

phase "checking trusted TLS and zone-scoped identity"
curl --silent --show-error --output /dev/null \
  "https://lampyr-readiness.default.$domain/"
ingress_inventory=$(kubectl --context "$context" -n kube-system get configmap lampyr-install \
  -o 'jsonpath={.data.ingress\.azure\.resources}')
role=$(jq -r .dnsRoleAssignmentId <<<"$ingress_inventory")
[[ $(az role assignment list --scope "$zone_id" \
  --query "[?id=='$role'].scope | [0]" -o tsv) == "$zone_id" ]]
first_serial=$(certificate_serial)

phase "forcing certificate reconciliation"
kubectl --context "$context" -n kourier-system delete secret lampyr-kourier-tls
for _ in $(seq 1 180); do
  serial=$(certificate_serial 2>/dev/null || true)
  if [[ -n $serial && $serial != "$first_serial" ]]; then
    break
  fi
  sleep 5
done
[[ -n ${serial:-} && $serial != "$first_serial" ]]
for _ in $(seq 1 180); do
  served_serial=$(endpoint_serial 2>/dev/null || true)
  [[ $served_serial == "$serial" ]] && break
  sleep 5
done
[[ ${served_serial:-} == "$serial" ]]
"$repo_root/build/ly" cluster health --provider azure --name "$cluster" --context "$context"

phase "changing the Knative domain without removing the working certificate first"
jq --arg domain "$changed_domain" '.domain=$domain' \
  "$project/provider.json" >"$project/provider.changed.json"
mv "$project/provider.changed.json" "$project/provider.json"
secret_preserved=true
timeout 30m "$repo_root/build/ly" cluster install \
  --provider azure \
  --provider-config "$project/provider.json" \
  --workspace "$project" \
  --name "$cluster" \
  --approve-provider-manifests &
install_pid=$!
while kill -0 "$install_pid" 2>/dev/null; do
  certificate_serial >/dev/null 2>&1 || secret_preserved=false
  sleep 5
done
wait "$install_pid"
[[ $secret_preserved == true ]]
changed_serial=$(certificate_serial)
[[ $changed_serial != "$serial" ]]
kubectl --context "$context" -n kourier-system get secret lampyr-kourier-tls \
  -o jsonpath='{.data.tls\.crt}' |
  base64 --decode |
  openssl x509 -noout -ext subjectAltName |
  grep -Fq "*.$changed_domain"
records_after_change=$(az network dns record-set a list \
  --subscription "$zone_subscription" \
  --resource-group "$zone_group" \
  --zone-name "$zone_name" \
  --query '[].name' -o tsv)
for retired in "$old_record" "$(default_record "$old_record")"; do
  if [[ $retired != "$new_record" &&
    $retired != "$(default_record "$new_record")" ]]; then
    if grep -Fqx "$retired" <<<"$records_after_change"; then
      echo "retired DNS record remains after domain change: $retired" >&2
      exit 1
    fi
  fi
done
curl --silent --show-error --output /dev/null \
  "https://lampyr-readiness.default.$changed_domain/"

phase "uninstalling and checking owned cleanup"
"$repo_root/build/ly" cluster uninstall \
  --provider azure \
  --provider-config "$project/provider.json" \
  --name "$cluster" \
  --context "$context" \
  --approve-provider-manifests
uninstalled=true
az resource show --ids "$zone_id" >/dev/null
remaining_records=$(az network dns record-set a list \
  --subscription "$zone_subscription" \
  --resource-group "$zone_group" \
  --zone-name "$zone_name" \
  --query '[].name' -o tsv)
for record in "$old_record" "$(default_record "$old_record")" \
  "$new_record" "$(default_record "$new_record")"; do
  if grep -Fqx "$record" <<<"$remaining_records"; then
    echo "owned DNS record remains: $record" >&2
    exit 1
  fi
done
phase "Azure ingress integration succeeded"
