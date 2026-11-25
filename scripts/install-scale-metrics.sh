#!/usr/bin/env bash
set -euo pipefail

if [[ $# != 1 || -z $1 ]]; then
    echo "usage: bash scripts/install-scale-metrics.sh CONTEXT" >&2
    exit 1
fi
context=$1
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

kubectl --context "$context" create --dry-run=client --validate=false \
    --filename "$root/k8s/observability.yaml" --output json |
    jq --slurp --exit-status '
        {
            apiVersion: "v1",
            kind: "List",
            items: [
                .[] | (if .kind == "List" then .items[] else . end) |
                select(.metadata.namespace != "lampyr-system") |
                select(.kind != "NetworkPolicy" or .metadata.name != "prometheus") |
                select(.metadata.name | IN(
                    "lampyr-observability",
                    "lampyr-observability-prometheus",
                    "lampyr-observability-kube-state-metrics",
                    "prometheus",
                    "kube-state-metrics"
                ))
            ]
        } |
        if (.items | length) == 0 then error("no scale metrics resources found") else . end
    ' |
    kubectl --context "$context" apply --filename -

for deployment in prometheus kube-state-metrics; do
    kubectl --context "$context" --namespace lampyr-observability \
        rollout status "deployment/$deployment" --timeout=300s
done
