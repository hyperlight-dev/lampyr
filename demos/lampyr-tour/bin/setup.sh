#!/usr/bin/env bash
# Prepare a Lampyr Kind installation for the tour: deploy the app, install
# Prometheus and Grafana, and create the container baseline. Safe to run again.
#
#   demos/lampyr-tour/bin/setup.sh          set up, or update
#   demos/lampyr-tour/bin/setup.sh --stop   stop the port-forwards it started
#
# NAME       Lampyr installation name (default: lampyr, as `ly cluster install`)
# CONTEXT    Kubernetes context (default: kind-$NAME)
# REGISTRY   where `ly deploy` pushes the app (default: the installation's
#            local registry, from ~/.lampyr/installations/kind/$NAME)
# PROMETHEUS_PORT, GRAFANA_PORT   local forward ports (default 19090, 13000)
# BURST      the runner's Fiber limit, for the tour's burst (default 1000,
#            at most 4096)
#
# Needs `ly`, kubectl, helm, curl and python3 on PATH.
set -euo pipefail

here=$(cd "$(dirname "$0")/.." && pwd)
name=${NAME:-lampyr}
context=${CONTEXT:-kind-$name}
prometheus_port=${PROMETHEUS_PORT:-19090}
grafana_port=${GRAFANA_PORT:-13000}
k() { kubectl --context "$context" "$@"; }

# The forwards outlive this script. Their PIDs and logs are kept in a
# private directory under $XDG_STATE_HOME (default ~/.local/state), which
# survives logging out, so a re-run replaces them and --stop stops only
# these.
state=${XDG_STATE_HOME:-$HOME/.local/state}/lampyr-tour-${context//\//_}
stop_forwards() {
  local pidfile pid cmdline
  [ -O "$state" ] || return 0
  for pidfile in "$state"/*.pid; do
    [ -f "$pidfile" ] || continue
    pid=$(cat "$pidfile")
    # Only a port-forward this script started for this context: the PID
    # may have been reused since.
    cmdline=$(tr '\0' ' ' 2>/dev/null <"/proc/$pid/cmdline" || true)
    if grep -qF -- "kubectl --context $context " <<<"$cmdline" && grep -qF -- " port-forward " <<<"$cmdline"; then
      kill "$pid" 2>/dev/null || true
      for _ in $(seq 50); do kill -0 "$pid" 2>/dev/null || break; sleep 0.1; done
    fi
    rm -f "$pidfile"
  done
}
if [ "${1:-}" = --stop ]; then
  ls "$state"/*.pid >/dev/null 2>&1 || echo "setup.sh: no forwards found for $context"
  stop_forwards
  exit 0
fi

burst=${BURST:-1000}
if ! [[ $burst =~ ^[1-9][0-9]*$ ]] || [ "$burst" -gt 4096 ]; then
  echo "setup.sh: BURST must be from 1 to 4096 (ly scale's limit), not '$burst'" >&2
  exit 2
fi

for tool in ly kubectl helm curl python3; do
  command -v "$tool" >/dev/null || { echo "setup.sh: $tool is not on PATH" >&2; exit 1; }
done

if [ -z "${REGISTRY:-}" ]; then
  settings=${LAMPYR_HOME:-$HOME/.lampyr}/installations/kind/$name/host/platform-settings.json
  [ -f "$settings" ] ||
    { echo "setup.sh: no Kind installation named $name; set NAME, or REGISTRY for another provider" >&2; exit 1; }
  REGISTRY=localhost:$(python3 -c 'import json, sys; print(json.load(open(sys.argv[1])).get("registryPort", 5002))' "$settings")
fi

echo "== deploying the runner and the playground to $context (images go to $REGISTRY)"
# The runner first, so the playground's binding has a Service to reach as
# soon as the playground is up. Both are public (app/lampyr.workspace.yaml),
# so the tour can call the runner directly.
for service in runner playground; do
  echo "   building and deploying the $service"
  # ly deploy takes only an artifact with the helper's facts, which ly build
  # records from the build/hluk-helper that use-cluster-inputs put there.
  ly build "$here/app/services/$service" >/dev/null
  ly deploy --context "$context" --image "$REGISTRY/$service:tour" "$here/app/services/$service" |
    awk '$1 == "url" && $2 ~ /^https?:/ {print "   " $2}'
done
# A re-run leaves older Revisions, each with its own home pod. ly deploy has
# waited for the new ones to be ready, so the old ones can go now.
CONTEXT=$context "$here/bin/prune-revisions"
# The tour's burst runs $burst Fibers on the runner's one home pod, and on a
# small node they need more than fiber-router's default 3 s to all start.
# Right after a deploy, the new Revision's capacity grant may still be
# settling, and the first attempt can fail; try once more after a pause.
ly scale --context "$context" --max-fibers "$burst" "$here/app/services/runner" >/dev/null 2>&1 || {
  sleep 5
  ly scale --context "$context" --max-fibers "$burst" "$here/app/services/runner" >/dev/null
}
# Set -acquire-timeout among fiber-router's other arguments, if it has any,
# and change the Deployment only when the value differs. This applies to
# every Service on the installation.
args=$(k -n lampyr-system get deploy fiber-router -o jsonpath='{.spec.template.spec.containers[0].args}')
patch=$(python3 -c '
import json, sys
current = json.loads(sys.argv[1]) if sys.argv[1] else []
wanted, skip = ["-acquire-timeout=120s"], False
for arg in current:
    if skip:
        skip = False
    elif arg.lstrip("-") == "acquire-timeout":
        skip = True  # "-acquire-timeout 3s": the value is the next argument
    elif not arg.lstrip("-").startswith("acquire-timeout="):
        wanted.append(arg)
if wanted != current:
    print(json.dumps([{"op": "add", "path": "/spec/template/spec/containers/0/args", "value": wanted}]))
' "$args")
if [ -n "$patch" ]; then
  k -n lampyr-system patch deploy fiber-router --type json -p "$patch" >/dev/null
  k -n lampyr-system rollout status deploy fiber-router --timeout=180s >/dev/null
fi
echo "   runner: up to $burst Fibers; a request waits up to about 240 s for one (twice -acquire-timeout)"

echo "== installing Prometheus and Grafana (namespace monitoring)"
helm repo add --force-update prometheus-community https://prometheus-community.github.io/helm-charts >/dev/null
helm repo update prometheus-community >/dev/null
helm upgrade --install monitoring prometheus-community/kube-prometheus-stack \
  --version 92.21.1 --kube-context "$context" --namespace monitoring --create-namespace \
  --values "$here/observability/values.yaml" --wait --timeout 10m >/dev/null
k apply -f "$here/observability/router-scrape.yaml" -f "$here/observability/dashboard.yaml" >/dev/null

echo "== creating the container baseline (namespace baseline)"
# The namespace first, from the manifest's own first document, so its
# labels (the pod security level) are applied the same way both times.
awk '/^---$/ {exit} {print}' "$here/baseline/runner-pods.yaml" | k apply -f - >/dev/null
# The server first, so new replicas start with the current one.
k -n baseline create configmap runner \
  --from-file=server.js="$here/app/services/runner/server.js" \
  --dry-run=client -o yaml | k apply -f - >/dev/null
# The manifest asks for 0 replicas; apply a running baseline's own count,
# and the server's checksum, so a changed server rolls the replicas.
replicas=$(k -n baseline get deploy runner --ignore-not-found -o jsonpath='{.spec.replicas}')
checksum=$(sha256sum "$here/app/services/runner/server.js" | cut -d' ' -f1)
sed -e "s/^  replicas: 0\$/  replicas: ${replicas:-0}/" -e "s/SERVER_SHA256/$checksum/" \
  "$here/baseline/runner-pods.yaml" | k apply -f - >/dev/null

mkdir -p "$state"
chmod 700 "$state"
stop_forwards
forward() { # NAME SERVICE LOCAL REMOTE READY_PATH
  local pidfile=$state/$1.pid log=$state/$1.log
  if (exec 3<>"/dev/tcp/127.0.0.1/$3") 2>/dev/null; then
    echo "setup.sh: 127.0.0.1:$3 is already in use; set ${1^^}_PORT" >&2
    return 1
  fi
  # exec, so the recorded PID is kubectl's own and cleanup can stop it.
  (exec kubectl --context "$context" -n monitoring port-forward "svc/$2" "$3:$4" >"$log" 2>&1) &
  local pid=$!
  echo "$pid" >"$pidfile"
  for _ in $(seq 60); do
    if ! kill -0 "$pid" 2>/dev/null; then
      echo "setup.sh: the $1 port-forward exited:" >&2
      tail -5 "$log" >&2
      rm -f "$pidfile"
      return 1
    fi
    curl -fs -m 2 -o /dev/null "http://127.0.0.1:$3$5" && return 0
    sleep 1
  done
  echo "setup.sh: $1 did not answer on 127.0.0.1:$3:" >&2
  tail -5 "$log" >&2
  kill "$pid" 2>/dev/null || true
  rm -f "$pidfile"
  return 1
}
echo "== forwarding Prometheus to 127.0.0.1:$prometheus_port and Grafana to 127.0.0.1:$grafana_port"
forward prometheus monitoring-kube-prometheus-prometheus "$prometheus_port" 9090 /-/ready
forward grafana monitoring-grafana "$grafana_port" 80 /api/health

domain=$(k -n kube-system get configmap lampyr-install -o jsonpath='{.data.domain}')
[ -n "$domain" ] || { echo "setup.sh: the lampyr-install ConfigMap has no domain; is Lampyr installed on $context?" >&2; exit 1; }
port=$(k -n kube-system get configmap lampyr-install -o jsonpath='{.data.httpsPort}')
[ "$port" != 443 ] || port=""
echo
echo "export REGISTRY=$REGISTRY"
[ "$prometheus_port" = 19090 ] || echo "export PROMETHEUS=http://127.0.0.1:$prometheus_port"
echo "export RUNNER=https://runner.default.$domain${port:+:$port}"
echo "export PLAYGROUND=https://playground.default.$domain${port:+:$port}"
echo "Grafana: http://127.0.0.1:$grafana_port, dashboard \"Lampyr\" (anonymous viewer; admin password lampyr-demo)"
echo "Stop the forwards: $0 --stop"
