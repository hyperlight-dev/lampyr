---
title: Separate Service Handlers from vendored Runtime Adapters
description: Keep Service Handlers separate from editable Runtime Adapters.
---

# Separate Service Handlers from vendored Runtime Adapters

**Status:** Proposed

The design separates user-owned Service Handlers from Runtime Adapters. Lampyr
creates Runtime Adapters under `.lampyr-adapter/`. Users can inspect and edit
these files. The adapter contains the protocol server, root file system build
logic, and language-native handler types.

The service owner retains `lampyr.yaml` and the final `Dockerfile`. This lets
service owners focus on application behavior. It also keeps generated code
visible and editable.

Canonical Service Templates are not stored in `docs/`. Lampyr generates
`docs/examples` from these templates and checks that the examples match.
Service owners upgrade adapters only when they run
`ly scaffold service NAME --upgrade`. A clean merge updates the adapter. A
conflict preview does not write files. The `--write-conflicts` option writes
markers for resolution. Builds do not rewrite adapter source.

The first contract supports Node.js, Python, Go, Rust, and .NET. Wasm is
deferred to [#2](https://github.com/hyperlight-dev/lampyr/issues/2). Bring
Your Own Services are
[Standalone Applications](standalone-applications.md). gRPC needs a
separate interface design.

This guest-side adapter is a step toward
[#6](https://github.com/hyperlight-dev/lampyr/issues/6). That work can define
a stable Hyperlight host-function ABI. The ABI can change host behavior without
rebuilding user binaries or service images.

## Service files and ownership

An ordinary generated service uses this layout:

```text
service/
├── handler.<language>          # user-owned Service Handler
├── lampyr.yaml                 # user-owned service policy
├── Dockerfile                  # user-owned final image packaging
└── .lampyr-adapter/            # tracked, generated, editable adapter
    ├── adapter.json            # schema, runtime, version, and hashes
    ├── server.<language>       # Runtime Adapter implementation
    ├── types.<language>        # language-native handler types
    ├── Dockerfile.rootfs       # root file system build logic
    └── baseline/               # pristine files for three-way upgrades
```

`.lampyr/` is ignored machine state. It contains build output, artifacts, and
runtime data. Lampyr can recreate it.

`.lampyr-adapter/` contains customer source. Its files contribute to source and
revision digests.

Canonical templates live under `templates/services/<runtime>/`.
`manifest.json` assigns user-owned, adapter-owned, and machine-state files.
CI checks generated `docs/examples` against these templates.

Optional `snapshotWarmup` metadata supplies runtime code to `hluk --warm-exec`.
Node.js initializes Fastify routes before snapshotting; handlers stay unloaded.

## Handler contract

The contract has the same behavior in each language. Names and types follow
each language's conventions:

- `handle` is required. It receives a Request and Context.
- A Response has a status, repeated header fields, and a byte-stream body.
  JSON, text, and byte helpers construct responses without another hook.
  HTTP request bodies stream too. These semantics follow WASI HTTP without
  requiring its ABI or resource-handle API.
- An event hook is optional. It receives an event and ProtocolContext.
- A WebSocket hook is optional. It receives a session and ProtocolContext.
- HTTP Context carries invocation cancellation. ProtocolContext also carries
  the originating request's method, path, and repeated headers, not its body.
- If an optional hook is missing, the adapter returns an unsupported or
  not-found response.
- If `events:` is set, the service must have an event hook at build time.
- If `events:` is not set, the event hook can still receive direct calls.
  Provider Event Sources remain disabled because no delivery policy exists.

The adapter connects maintained server libraries to the hooks. The libraries
handle HTTP parsing, WebSocket framing, and connection I/O. The adapter applies
CloudEvent conversion, cancellation, backpressure, error mapping, shutdown,
and acknowledgment rules. Node.js uses Fastify and `@fastify/websocket` with
`ws`. Python uses aiohttp. Dependency locks are adapter-owned source files.
The root file system build installs them without host runtime tools.

The [shared handler contract](handler-contract.md) describes the behavior. It
also defines Node.js and Python hook types and conformance fixtures.

The first implementation supports Node.js, Python, Go, Rust, and .NET. Wasm
needs a WASI-HTTP/WIT handler contract. gRPC is not in this contract.

## Service manifest

An ordinary generated service uses these settings:

- Do not add a runtime `command`. The entrypoint is in `adapter.json`.
- Do not add a `protocols` block.
- Set `events:` only when provider-managed event delivery is required.
- Keep runtime, resources, and execution policy in `lampyr.yaml`.

This design does not define network authorization or generated Kubernetes
NetworkPolicies. A separate, explicit policy must define those rules. The
adapter must not infer them from handler hooks.

## Canonical templates

Canonical template files are stored here:

```text
templates/services/<runtime>/
├── project/                    # files owned by the user
└── adapter/                    # files generated by the adapter
```

`ly scaffold service` reads these templates. `docs/examples` contains generated
output. Each example must match the scaffold for its runtime. CI regenerates
each example and fails if a file changes.

Each `manifest.json` assigns every template file to an ownership class:

- **user-owned**: create the file once; do not rewrite it.
- **adapter-owned**: generate and track the file; allow edits and three-way
  upgrades.
- **machine state**: ignore and recreate the file as needed.

`templateVersion` in `manifest.json` orders adapters: upgrades refuse to move an
adapter to an older version, and drift warnings tell an older `ly` to upgrade
itself. Any change to an adapter-owned file, the entrypoint, or the snapshot
warmup therefore needs a new `templateVersion`. The CLI test
`adapter_content_changes_require_a_template_version_bump` fails until the new
version and its content digest are added to
`cli/tests/fixtures/adapter-template-versions.json`. Never change an existing
entry.

## Explicit adapter upgrades

Run `ly scaffold service NAME --upgrade` to upgrade an adapter. The command
resolves the service name in the same way as service creation. A value that
contains `/` or starts with `.` is a path to the service directory instead.

The command merges three versions:

1. The tracked pristine baseline.
2. The service's current editable adapter.
3. The installed Service Template.

If all files merge without conflict, Lampyr writes the new adapter and
baseline. If a file conflicts, the default command prints a red and green diff,
returns an error, and writes nothing. The `--write-conflicts` option writes
conflict markers and pending-upgrade metadata. It also returns an error.

After you resolve the files, run the same upgrade command again. Lampyr checks
the resolution, installs the saved target baseline, and removes pending state.

`adapter.json` records a `pristine` object with the template's `entrypoint`
and `snapshotWarmup`, the merge base for those editable fields. A `ly` built
before this field existed rejects such metadata as an unknown field; upgrade
`ly`. Metadata without `pristine` is merged against the installed template when
its `templateVersion` and tracked file hashes match it; otherwise its
`entrypoint` and `snapshotWarmup` must equal the template's.

A conflict-writing upgrade adds `pending.json` and `pending-baseline/`. Pending
metadata stores the full target metadata, the conflict paths, a digest of the
original `adapter.json`, and digests of the cleanly merged files. Lampyr checks
the pending baseline against the target hashes, and refuses to finish if the
merged files changed, for example after a version-control restore.

The saved metadata pins the target version and files. If the installed template
changes before resolution is complete, Lampyr still finalizes the saved
version. It prints a warning. A later upgrade can install the new version.

The default conflict preview writes no files. A clean upgrade stages the
complete adapter in a temporary directory. On Linux, Lampyr installs it with
atomic directory exchange. Lampyr locks cooperating upgrades, limits file
access, rejects symlinks, checks pristine files, and checks for concurrent
changes.

Build, dev, and deploy do not upgrade source. They report if the adapter is
modified or older than the installed template. They continue with a warning,
but do not guarantee support for customized adapters.

## Implementation sequence

1. **Define template ownership.**
   - Add template manifests and runtime template directories.
   - Move scaffold inputs out of `docs/examples`.
   - Generate examples from canonical templates and check them in CI.
2. **Add adapter metadata and files.**
   - Generate `.lampyr-adapter/`, its baseline files, and `adapter.json`.
   - Keep the final `Dockerfile` in the service root.
   - Add adapter files to source, artifact, and revision digests.
3. **Define the handler contract.**
   - Specify Request, Response, Context, Event, and WebSocket session behavior.
   - Define errors, cancellation, streaming, and acknowledgments.
   - Add shared cross-language test cases.
4. **Implement language adapters.**
   - Add idiomatic templates and local generated types for Node.js, Python, Go,
     Rust, and .NET.
   - Move server and root file system build logic out of user handlers.
   - Check required and optional hooks.
5. **Simplify manifests and builds.**
   - Store the entrypoint in adapter metadata.
   - Remove `command` and `protocols` from ordinary scaffolded services.
   - Use `events:` to control provider event policy.
   - Build root file systems with `.lampyr-adapter/Dockerfile.rootfs`.
6. **Add explicit upgrades.**
   - Merge clean changes and show colorized conflict previews.
   - Add `--write-conflicts` and pending state.
   - Finalize resolved upgrades on a later command run.
   - Report adapter changes without rewriting source.
7. **Update documentation and test behavior.**
   - Regenerate committed examples.
   - Test HTTP values, streams, events, WebSockets, errors, cancellation,
     adapter edits, and upgrade conflicts.
   - Update scaffold, build, and runtime compatibility documentation.

## Future work

- [#6](https://github.com/hyperlight-dev/lampyr/issues/6) can move adapter
  behavior behind Hyperlight host functions. A stable ABI can change host
  behavior without rebuilding user binaries or service images.
- [#2](https://github.com/hyperlight-dev/lampyr/issues/2) defines the
  Wasm/WASI-HTTP handler contract.
- [Standalone Applications](standalone-applications.md) cover Bring
  Your Own Services. These services keep their own server and build contract.
