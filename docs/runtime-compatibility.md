---
title: Runtime and protocol compatibility
description: Protocol checks in the pull-request Kind test.
---

# Runtime and protocol compatibility

Kind integration coverage for Handler templates:

| Runtime | Tier | HTTP | Streaming | WebSocket | Events |
|---|---:|---:|---:|---:|---:|
| Node.js | 1 | pass | pass | pass | pass |
| Python | 1 | pass | local contract tests | pass | pass |
| Go | 1 | pass | pass | pass | pass |
| Rust | 1 | pass | pass | pass | pass |
| .NET | 1 | pass | pass | pass | pass |

Kind tests also cover activation and lifecycle. The
[handler contract](designs/handler-contract.md) has native tests in
`just examples-check` and Hyperlight tests in `just test-examples-local`.
These cover streaming uploads, repeated headers, events, and WebSockets
for all five runtimes.

## Standalone Applications

The paired `hello-<runtime>-standalone` examples use Express, FastAPI/Uvicorn,
Gin, Axum, and ASP.NET Core Minimal APIs. The local Hyperlight and Kind suites
check startup on `$PORT`, one JSON route, and the same build/deploy/route/scale/
status/logs/audit/delete lifecycle as Handler Applications.

Standalone servers own their protocol behavior. Lampyr passes HTTP, streaming,
and WebSockets without manifest flags; the minimal templates demonstrate only
ordinary HTTP, not framework middleware, events, or richer protocols. An
`events:` block enables provider delivery policy without a hook check.
Wasm support is deferred until a handler contract is defined.

## gRPC

Handler and Standalone Applications route HTTP/1.1, streaming, and WebSockets.
Their manifests cannot set `protocols`; gRPC is deferred until its interface
is designed.
