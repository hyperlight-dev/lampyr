---
title: Service Handler contract
description: Native hooks and streaming HTTP semantics.
---

# Service Handler contract

Handlers own application behavior; adapters own transport and lifecycle. The
contract covers Node.js, Python, Go, Rust, and .NET. Wasm is deferred to
[#2](https://github.com/hyperlight-dev/lampyr/issues/2).

| Hook | Node.js | Python | Go | Rust | .NET |
|---|---|---|---|---|---|
| HTTP (required) | `handle(request, context)` | `handle(request, context)` | `Handle` | `Hooks::new(handle)` | `IHandler` |
| Event | `handleEvent(event, context)` | `handle_event(event, context)` | `HandleEvent` | `.event(handle_event)` | `IEventHandler` |
| WebSocket | `handleWebSocket(session, context)` | `handle_websocket(session, context)` | `HandleWebSocket` | `.websocket(handle_websocket)` | `IWebSocketHandler` |
| Types | `.lampyr-adapter/types.d.ts` | `lampyr_types` | `types.go` | `types.rs` | `Types.cs` |
| Cancellation | `context.signal` | `context.cancelled` | `context.Context` | `context.cancelled` | `context.CancellationToken` |

Go sets the exported `Service` variable. Rust returns `Hooks` from `hooks()`.
.NET implements the interfaces on `Handler`. Missing optional hooks return
HTTP 404.

## HTTP

Request has `method`, `path` (including query), `headers`, and a `body` stream.
Response has `status` (default 200), `headers`, and a `body` stream. Bodies are
single-consumer byte streams; uploads stream without buffering. Request bodies
and WebSocket messages are limited to 1 MiB.

Use `Response.json`, `Response.text`, or `Response.bytes` (`JSON`, `Text`,
`Bytes` in Go) for complete values, or wrap byte chunks for streams. Plain
values are not response bodies. Body readers (`bytes`, `text`, `json`) default
to 1 MiB: invalid UTF-8/JSON returns 400, over the limit returns 413.

```javascript
const {Response} = require('./.lampyr-adapter/types');

async function handle(request, context) {
  return Response.json(await request.body.json());
}
```

```go
func Handle(request Request, context Context) (Response, error) {
	var value any
	if err := request.JSON(&value); err != nil {
		return Response{}, err
	}
	return JSON(value)
}
```

Headers preserve repeated fields. The adapter owns framing headers and
backpressure; HEAD, 204, and 304 send no body. Every response the handler or
adapter produces, including adapter errors, carries
`X-Content-Type-Options: nosniff` unless the handler sets that header itself,
so browsers never sniff handler bytes into HTML. A response without a
`Content-Type` is never given one guessed from its bytes.

## Context and failures

HTTP Context carries cancellation only. Event and WebSocket hooks receive a
ProtocolContext whose `request` has the originating method, path, and headers.
Cancellation is cooperative: check it in long-running handlers.

Return error Responses or throw `HandlerError(status, message)`. Unexpected
errors are logged and return 500. Stream failures after headers close the
connection.

## WebSockets

Adapters accept WebSocket upgrades from any `Origin`, and the hook runs after
the upgrade completes. A handler that relies on cookies or other ambient
credentials must check `Origin` in the ProtocolContext request headers and close
the session before acting on them, or it is open to cross-site WebSocket
hijacking.

When a peer sends a Close frame, the session ends. Messages the peer sent
before the Close that the handler has not received yet may be dropped, depending
on the runtime. Acknowledge anything that must not be lost at the application
level.

## Events

CloudEvents binary headers or `application/cloudevents+json` select the event
hook on any path. Fields: `id`, `type`, `source`, `subject`, `time`, `data`,
`attempt` (default 1). JSON data becomes a native value; other data stays text.
Malformed events, invalid UTF-8, and `data_base64` return 400.

Only a 2xx response acknowledges an event. Adapters do not retry or write
dead-letter files.

`events:` enables managed delivery and requires an event hook at build time.
Without it, direct calls work but provider Event Sources cannot bind. See
[`ly event`](../cli/event.md).

## Build and tests

`ly build` and `ly dev` check hooks and install dependencies in Docker; no host
language toolchain is needed. Builds never rewrite adapter source or metadata.

`just examples-check` runs the native contract tests for all five runtimes;
`just test-examples-local` also runs them in Hyperlight. They exercise
generated servers, not mocks.
