---
title: Walkthrough
description: Create, run, deploy, and operate a first Lampyr Service on Kind.
shell: bash
timeout: 600
---

# Walkthrough

Create a Lampyr Service, run it locally, and deploy it to Kind.

## Before you start

The `ly` CLI must be on your `PATH`. Install it from a release as the
[README](https://github.com/hyperlight-dev/lampyr#install-the-cli) shows, or follow
[Build from source](contributing.md#build-from-source). The
[local requirements](requirements.md) include Docker with Buildx and read/write
access to KVM; `ly doctor` checks those and the remaining build requirements.

Check the local prerequisites first:

```bash markmason
ly doctor
```

Output:

```expected-contains
Lampyr doctor
[ok] ly 0.1.1
[ok] platform $REPO_ROOT
Ready.
```

Scaffold a temporary Workspace in the current directory:

```bash markmason
workspace=$(mktemp -d "$PWD/.lampyr-walkthrough.XXXXXX")
ly scaffold workspace "$workspace"
cd "$workspace"
cluster="lampyr-walkthrough-$(printf '%s' "${workspace##*.}" | tr '[:upper:]' '[:lower:]')"
registry_port=5500
printf 'registryPort: %s\n' "$registry_port" > provider.yaml
```

## Create a Kind cluster

With `ly` installed from a release, skip to `ly cluster install` below: the
release's platform carries everything the cluster needs.

From a checkout, prepare the guest and runtime images once, outside installation:

```bash
just kind::build cli::runtime-images
```

A checkout also has no fiberd-k8s-controller or grant Pod images to install
by default, as a release's platform has. Build them, and serve them from the
local registry this walkthrough's cluster uses:

```bash
just upstream::cluster-inputs build/cluster-inputs
LAMPYR_CLUSTER_INPUTS=$PWD/build/cluster-inputs just use-cluster-inputs 5500
```

The [Kind cluster guide](cluster-kind.md#install) says what these do.

Create the local kind cluster and install Lampyr.
See the [Kind cluster guide](cluster-kind.md) for prerequisites and configuration
options. To use AKS instead, follow the [AKS cluster guide](cluster-aks.md).

```bash markmason
ly cluster install --provider kind --name "$cluster" \
  --provider-config provider.yaml \
  --allow-docker
```

Output:

```expected-substring
"installed":true
```

## Inspect the default app

`ly scaffold workspace` creates a default Lampyr Service at `services/app`.
You can add more services later with [`ly scaffold service`](cli/scaffold.md#create-a-service).
For now, inspect the generated app manifest, handler, and Dockerfile:

```bash markmason
ls -1 services/app/Dockerfile services/app/handler.js services/app/lampyr.yaml
```

Output:

```expected
services/app/Dockerfile
services/app/handler.js
services/app/lampyr.yaml
```

The generated handler already emits a structured line for each request, so it
will appear later in `ly logs` without any edits.

## Run locally
Run the app on your local machine. It will build the project.

```bash markmason
ly dev --port 8080 services/app > dev.log 2>&1 &
dev_pid=$! #save this so we can shut it down later
```

Now curl the endpoint:
```bash markmason
curl --fail --retry 60 --retry-connrefused --retry-delay 1 \
  -H 'Host: app.default.localhost' \
  http://127.0.0.1:8080/test
```

Output:

```expected
{"message":"hello from Hyperlight edge","path":"/test"}
```

## Build and deploy

Stop the local development server before building:

```bash markmason
kill "$dev_pid"
wait "$dev_pid" || true
dev_pid=
```

Here is the trick: `ly build` packages the app, prepares its Hyperlight
environment, and captures a ready-to-restore snapshot. New instances start
from that pre-warmed point instead of assembling everything from scratch,
which improves cold-boot latency.

The application handler does not run during the build; it starts after the
snapshot is restored. `ly build` also records the facts of the platform's
helper, from which `ly deploy` seals and pushes the fiberd template the
cluster restores the snapshot from.

```bash markmason
ly build services/app
```

Then deploy it to Kind:

```bash markmason
ly deploy \
  --image "localhost:$registry_port/app:v1" \
  services/app
```

Output:

```expected-regex
deployed service app
revision app-.+
image localhost:5500/app@sha256:[0-9a-f]{64}
visibility external
url https://app\.default\.localhost:\d+
```

Deploy publishes the built image to a registry if needed. See [`ly build`](cli/build.md) and
[`ly deploy`](cli/deploy.md) for artifact, snapshot, and deployment options.
Kind assigns the local ingress port dynamically, so use the URL printed by `ly deploy`.

## Test it on KIND

Start a port forward in the background:

```bash markmason
kubectl port-forward -n kourier-system svc/kourier 8080:80 > forward.log 2>&1 &
forward_pid=$!
```

Send a request with the deployed service's hostname:

```bash markmason
curl --fail --silent --show-error --retry 300 --retry-connrefused \
  --retry-delay 1 --max-time 5 \
  -H 'Host: app.default.localhost' http://127.0.0.1:8080/test
```

Output:

```expected
{"message":"hello from Hyperlight edge","path":"/test"}
```

## Observe and clean up

Inspect the deployed app:

```bash markmason
ly status services/app
```

Output:

```expected-regex
service app
revision app-.+
application handler
url https://app\.default\.localhost:\d+
traffic app-.+=100%
capacity app-.+ ready, max \d+ fibers
```

- **service** is the logical Lampyr Service.
- **revision** is the immutable version created by the deployed artifact. A
  later build and deploy creates a new revision.
- **application** is the application model in the local `lampyr.yaml`:
  `handler` for this scaffolded app.
- **url** is the routed endpoint. Local Kind uses a dynamically assigned port.
- **traffic** shows how requests are divided between revisions. The first
  deployment sends 100% to the current revision.
- **capacity** reports the revision's CapacityGrant: `ready` once its home Pod
  serves Fibers, and the most Fibers it may run at once.

See [`ly status`](cli/status.md), [`ly scale`](cli/scale.md), and
[`ly deploy`](cli/deploy.md) to explore runtime state, capacity, and revision
traffic.

Now view the structured line written by `handler.js`:

```bash markmason
ly logs --tail 10 services/app
```

Output:

```expected-substring
"file":"handler.js","kind":"handler","service":"app","message":"request received","path":"/test"
```

Stop the background processes, delete the service and Kind cluster, and remove
the temporary Workspace:

```bash markmason always
cleanup_status=0
for pid in "${dev_pid:-}" "${forward_pid:-}"; do
  if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
    kill "$pid" 2>/dev/null || true
    wait "$pid" || true
  fi
done
if [ -n "${workspace:-}" ]; then
  if [ -f "$workspace/services/app/lampyr.yaml" ]; then
    ly delete "$workspace/services/app" || cleanup_status=$?
  fi
  if [ -f "$workspace/lampyr.workspace.yaml" ] && [ -n "${cluster:-}" ]; then
    (
      cd "$workspace"
      ly cluster uninstall --provider kind --name "$cluster" --delete-cluster
    ) || cleanup_status=$?
  fi
  cd "$workspace/.." || cleanup_status=$?
  rm -r -- "$workspace" || cleanup_status=$?
fi
test "$cleanup_status" -eq 0
```

For command options, use the [CLI reference](cli/README.md). See
[Kind cleanup](cluster-kind.md#remove-lampyr) for other cleanup options.

## Further reading

- [Azure Event Grid](azure-event-grid-source.md) to add event-driven delivery
  after deploying your first service.
- [Runtime examples](examples.md) for other language templates.
- [Runtime compatibility](runtime-compatibility.md) for protocol coverage and
  snapshot limitations.
- [`ly deploy`](cli/deploy.md) for new revisions, canary traffic, and rollback.
- [Lampyr Service model](lampyr-service.md) for how the deployed resources fit
  together.
