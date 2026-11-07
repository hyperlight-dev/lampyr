---
title: ly scaffold
---

# `ly scaffold`

Use `ly scaffold` to create a Workspace or a Lampyr Service from a template.
You own the generated Workspace or Service.

## Create a Workspace

```bash
build/ly scaffold workspace my-project
cd my-project
```

Lampyr creates this directory structure:

```text
my-project/
├── lampyr.workspace.yaml
├── services/
│   └── app/
├── infra/
└── k8s/
```

The `app` directory contains a Node.js service. You can edit this service. The
`k8s` directory contains copies of the shared Lampyr platform manifests. You
own these copies.

The workspace sets `ingress.services: [app]` in `lampyr.workspace.yaml`. Adding
more services keeps `app` as ingress until you edit that selection.

Each provider owns its provider-specific manifests. Lampyr does not copy these
manifests into the Workspace. A Workspace can contain more than one Lampyr
Service and more than one named infrastructure installation.

## Create a service

Create a service inside the Workspace:

```bash
build/ly scaffold service hello-edge
build/ly scaffold service hello-python --runtime python
```

Lampyr creates `services/hello-edge/`:

```text
services/hello-edge/
├── handler.js
├── lampyr.yaml
├── Dockerfile
├── .gitignore
└── .lampyr-adapter/
    ├── adapter.json
    ├── server.js
    ├── types.js
    ├── types.d.ts
    ├── package.json
    ├── package-lock.json
    ├── Dockerfile.rootfs
    └── baseline/
```

You own the handler, `lampyr.yaml`, the final `Dockerfile`, and `.gitignore`.
The `.lampyr-adapter/` directory contains editable runtime and build support.
The `baseline/` directory contains an unchanged copy of each generated adapter
file. Commit `.lampyr-adapter/` with the Service.

Handlers use the [handler contract](../designs/handler-contract.md). Add
`events:` to `lampyr.yaml` for provider delivery; direct event calls work
without it. The handler file is `handler.js`, `handler.py`, `handler.go`,
`handler.rs`, or `Handler.cs`.

Git ignores `.lampyr/`. This directory contains temporary machine state. You
can delete it.

The `adapter.json` file records the schema version, runtime, template version,
guest entrypoint, and a `sha256:` hash for each adapter file. Each path is
relative to `.lampyr-adapter/`. You may edit `entrypoint` and `snapshotWarmup`;
the `pristine` object keeps the template's values for upgrades.

The hashes identify the service-name-rendered files in `baseline/`. They do not
identify later edits. The source revision includes the metadata, baseline
files, and editable adapter files.

Use a lowercase Kubernetes DNS label of at most 43 characters for the Service
name: each Revision appends 13 characters, and its CapacityGrant name is
limited to 56. `ly build` and `ly deploy` refuse longer names. Before it creates
files, Lampyr checks for existing files and directories. It does not replace an
existing project file or adapter directory.

Outside a Workspace, the command creates `hello-edge/` in the current
directory. Workspace placement does not select the application model.

## Choose a runtime

Node.js is the default runtime. Use `--runtime` to select another runtime:

```bash
build/ly scaffold service hello-python --runtime python
build/ly scaffold service hello-go --runtime go
build/ly scaffold service hello-rust --runtime rust
build/ly scaffold service hello-dotnet --runtime dotnet
```

The Node.js, Python, Go, Rust, and .NET templates generate named application
hooks with type definitions. Their adapters process the protocols. Their
manifests declare `application.type: handler` and do not contain `command` or
`protocols`. The CLI gets the entrypoint from the adapter metadata.

The `events:` policy lets a provider control event delivery. This policy
requires an event hook when you build the Service. Without this policy, the
Service can still receive direct CloudEvent requests. A provider Event Source
cannot connect to the Service. See the
[handler contract](../designs/handler-contract.md).

Go and Rust handlers register required and optional hooks with defined types.
.NET uses a required handler interface and optional event and WebSocket
interfaces. The `.lampyr-adapter/` directory contains compiler inputs,
contract types, and the server. The Service root directory contains the
handler source file.

Go, Rust, and .NET handlers receive a byte stream for the request body.
They return a byte stream in the response.
Use explicit JSON, text, or byte helpers to create finite response bodies.
The adapter preserves repeated header values.
It does not read the full HTTP body before it calls the handler.

The `ly build` and `ly dev` commands compile or import handlers in containers.
They then check the packaged adapter before they prepare a VM.
If the policy requires an event hook, they also check for that hook.
You do not need Node.js, Python, Go, Rust, or .NET on the host.
The checks do not change tracked adapter source files.

## Create a Standalone Application

```bash
build/ly scaffold service inventory --runtime dotnet --standalone
```

`--standalone` selects a customer-owned listening process, not a different
deployment target. It supports Express (Node.js), FastAPI/Uvicorn (Python),
Gin (Go), Axum (Rust), and ASP.NET Core Minimal APIs (.NET). Each minimal
template contains one JSON route in an application file, separate from server
startup and port binding.

```yaml
application:
    type: standalone
    command: /app/Service
```

The command must listen on `0.0.0.0:$PORT`. Node.js and Python commands name
the script executed by the runtime; compiled runtimes name the executable.
All generated files, including the root-level `Dockerfile.rootfs`, dependency
manifests, and server startup, are customer-owned. No `.lampyr-adapter/` is
created or allowed. `--upgrade` refuses standalone services without changing
any files; it cannot be combined with `--standalone`.

All supported native runtime manifests require `application.type`; old manifest
forms without the field, including explicit top-level command and protocol
settings, are rejected. Explicit application manifests also reject top-level
`command`, `protocols`, unknown models, missing standalone commands, handler
commands, or an adapter alongside a standalone server. HTTP, streaming, and
WebSockets pass through without flags.
An optional `events:` block enables provider-managed delivery policy; Lampyr
trusts the customer's event endpoint rather than checking handler hooks.
Templates do not implement events, streaming, WebSockets, or middleware.
gRPC is deferred for these application models.

Build, snapshot, deploy, routing, scaling, status, logs, audit, and deletion use
the ordinary Lampyr lifecycle. Prebuilt OCI application images and arbitrary
Kubernetes workloads are not standalone inputs.

## Upgrade an adapter explicitly

```bash
build/ly scaffold service hello-python --upgrade
```

The command resolves the service name in the same way as service creation.
Inside a Workspace, it selects `services/hello-python`. Outside a Workspace,
it selects `./hello-python`. A value that contains `/` or starts with `.` is a
path to the service directory instead, for example
`build/ly scaffold service ./api --upgrade`. Build, dev, and deploy print this
path form in their upgrade hint.

The service's current runtime selects the installed template. You cannot use
`--runtime` with `--upgrade`. If the adapter was created by a newer `ly` (a
higher `templateVersion`), the upgrade refuses instead of downgrading it;
upgrade `ly` instead. The upgrade does not import a handler, start a
server, run Docker, or require a language SDK.

The upgrade supports Handler Applications that use adapter metadata.
Standalone Applications have no Runtime Adapter, so the upgrade refuses them.

Lampyr uses libgit2 to merge three versions of each adapter file:

1. The tracked pristine file in `baseline/`.
2. The current editable file in the service.
3. The installed template rendered with the service name.

Lampyr installs a clean merge immediately. The merge preserves customer
changes and ordinary file permissions. It also handles files that the template
adds or deletes. It merges `entrypoint` and `snapshotWarmup` in `adapter.json`
the same way: your edit is kept unless the template changed the same field, in
which case the upgrade stops before writing anything. To take the template's
value, set the field to it. To keep yours, set the same field under `pristine`
to the new template value, which records that you have seen the change. Then
rerun the upgrade.

Lampyr requires a resolution if a customer edit conflicts with a template
deletion, including an edit to file permissions only. It also requires a
resolution if both sides add different content at the same path. Lampyr keeps
untracked adapter files unless they collide with a template path.

Lampyr never upgrades the handler, `lampyr.yaml`, the final `Dockerfile`, or
`.gitignore`.

### Review conflicts without writing

By default, the command shows a unified diff and exits with an error if a merge
has conflicts. It does not change any byte in the service, including metadata
and baselines.

On a terminal, removed lines are red and added lines are green. Use
`--color always` to force color in captured or non-terminal output. Use
`--color never` to disable color. Automatic color respects `NO_COLOR`.

### Write conflicts and resolve them

Run this command to write conflict markers and save the upgrade target:

```bash
build/ly scaffold service hello-python --upgrade --write-conflicts
# The command exits with an error. Resolve the markers under .lampyr-adapter/.
# Delete a conflicted file if you want to keep the deletion.
build/ly scaffold service hello-python --upgrade
```

The first command exits with an error after it writes the conflict files.
It writes diff3 markers, `pending.json`, and `pending-baseline/` together.
It leaves `adapter.json` and `baseline/` unchanged.

The second command rejects unresolved markers. After you resolve them, run the
same command again. Lampyr preserves your resolution, including a file
deletion. It then promotes the saved target to `baseline/` and `adapter.json`,
and removes the pending state.

The pending state pins the template version and file contents. If the installed
template changes before you finish, Lampyr still finalizes the pinned target
and prints a warning. Run another upgrade to use the newer installed template.

Files that merged cleanly must still hold the merged content when you finish.
If you revert the adapter with version control, finishing stops instead of
recording the new template over the old files. To cancel a pending upgrade,
restore the adapter files and delete `.lampyr-adapter/pending.json` and
`.lampyr-adapter/pending-baseline/`. `git restore` leaves both behind because
they are new, untracked files.

Do not edit `adapter.json` while an upgrade is pending. Build, dev, deploy, and
`--upgrade` refuse until you restore it or cancel the upgrade; status, logs,
scale, and delete keep working.

### Adapter warnings and errors

Build, dev, and deploy warn if an adapter is modified or out of date. They
continue with a warning, but do not guarantee support for customized adapters.
They never rewrite adapter files. Different hashes for editable source do not
indicate corrupt metadata.

These commands, and status, logs, scale, and delete, read only `adapter.json`,
the files it tracks, their baselines, and any pending upgrade state. Other files
under `.lampyr-adapter/`, such as `target/`, `node_modules/`, caches, or editor
lock files, do not affect them. The file limits below still apply to the
tracked files. Only `--upgrade` reads the whole adapter directory and applies
the limits to every entry.

Corrupt metadata, changed pristine baselines, and unfinished upgrades are
errors for build, dev, and deploy. An unfinished upgrade does not block status,
logs, scale, or delete when its saved metadata and baseline are valid.
Restore trusted metadata and baselines from version control before you
build. The hashes check the merge base. They are not an authenticity signature.
Changing both the metadata and baseline can hide damage.

### Upgrade file and system limits

The upgrade requires Linux, `/proc`, and a filesystem that supports atomic
directory exchange with `renameat2(RENAME_EXCHANGE)`. Lampyr stages and checks
the complete adapter in a temporary directory next to the service. It syncs the
files and directories, then exchanges the adapter directories atomically.
Lampyr does not update metadata with a series of per-file writes.

Lampyr locks cooperating upgrades and checks for unexpected changes during the
exchange. Stop editors and build tools that write to the adapter while an
upgrade runs. Lampyr aborts or rolls back if it detects an unexpected change.
If rollback fails, the error identifies the saved recovery directory.

During `--upgrade`, adapter entries must be real files or directories with
relative UTF-8 paths. Remove local build output from `.lampyr-adapter/` first.
Paths cannot contain traversal, backslashes, control characters, or reserved
names. Symlinks and special files are not allowed. Each regular file must have
one link.

These limits bound file reads and staging:

| Item | Limit |
|---|---:|
| Metadata file | 256 KiB |
| Individual file | 4 MiB |
| Complete adapter tree | 64 MiB |
| Adapter entries | 2048 |
| Path length | 240 bytes |
| Path depth | 16 components |

Lampyr can merge binary files when one side is unchanged. If both sides change
a binary file, the command exits without writing. Reconcile that file manually.
File and directory collisions are errors. Lampyr does not overwrite them.

## Canonical templates

Canonical templates are in `templates/services/<runtime>/`. The
`manifest.json` file lists each `project/` file as user-owned and each
`adapter/` file as adapter-owned. The manifest also identifies `.lampyr/` as
machine state.
The adjacent `standalone/manifest.json` lists every `standalone/project/` file
as user-owned.

Files in `docs/examples` are generated scaffold examples. They are not template
inputs. The build checks that each example matches its template.

Edit the generated application. Then build it by path:

```bash
build/ly build services/hello-edge
```

See [Lampyr Service architecture](../lampyr-service.md) for the request path
and runtime architecture.
