---
title: Standalone Application plan
description: Support customer-owned servers through the normal Hyperlight Lampyr lifecycle.
---

# Standalone Application plan

Standalone Applications provide the escape hatch for customers who want
to own their application server rather than use Lampyr's generated Runtime
Adapter.

## Boundary

A Standalone Application is still a Lampyr Service:

- it is built from customer source;
- its root filesystem is snapshotted for Hyperlight;
- it is activated as Fibers through a Capacity Grant;
- it uses Lampyr routing, policy, observability, and lifecycle commands; and
- it must start a listening process bound to `0.0.0.0` on the port supplied
  through the `PORT` environment variable.

It is not an arbitrary Kubernetes Deployment or an existing container image
that bypasses Lampyr's snapshot artifact and isolation model.

## Manifest

The application model is explicit:

```yaml
apiVersion: lampyr.dev/v1alpha1
kind: Project
name: inventory
runtime: dotnet
application:
  type: standalone
  command: /app/inventory
resources:
  memory: 512Mi
  cpu: 250m
```

Handler Applications use:

```yaml
application:
  type: handler
```

Their command remains Runtime Adapter metadata rather than customer
configuration.

Manifest validation requires exactly one coherent model:

- `handler` requires a valid tracked `.lampyr-adapter/` and forbids a
  customer-owned command;
- `standalone` requires a nonempty command and does not use a Runtime Adapter;
  and
- ambiguous combinations fail before rootfs construction.

The current `protocols` block is not retained. HTTP, streamed responses, and
WebSockets traverse the listening process without separate capability flags.
The presence of `events:` enables provider-managed event delivery policy.
Lampyr trusts a Standalone Application to implement that endpoint because the
server is customer-owned.

## Scaffolding

Create a Standalone Application with:

```bash
ly scaffold service inventory --runtime dotnet --standalone
```

Name and Workspace resolution remain identical to ordinary service
scaffolding.

Canonical templates live beside Handler templates:

```text
templates/services/dotnet/
├── handler/
│   ├── project/
│   └── adapter/
└── standalone/
    └── project/
```

Every file under `standalone/project/` is copied once and becomes
customer-owned. `ly scaffold service --upgrade` applies only to Handler
Runtime Adapters and never rewrites Standalone Application source.

Standalone templates must not invent a Lampyr-specific application class,
service interface, or handler abstraction. They separate only:

- application routes and ordinary framework code; and
- server startup, port binding, and shutdown.

They are deliberately minimal bootstrap projects, not full framework starter
kits. Each template should contain one JSON route and only the dependencies
needed to start the selected mainstream framework. Lampyr does not maintain
template-level authentication, dependency injection patterns, middleware
stacks, OpenAPI, databases, application configuration systems, event handlers,
streaming, WebSockets, or production architecture.

## Language templates

The first implementation covers all non-Wasm runtimes:

| Runtime | Framework | Application routes | Server startup |
|---|---|---|---|
| Node.js | Express | `application.js` | `server.js` |
| Python | FastAPI | `application.py` | `server.py` using Uvicorn |
| Go | Gin | `application.go` | `main.go` |
| Rust | Axum | `src/application.rs` | `src/main.rs` |
| .NET | ASP.NET Core Minimal APIs | `Application.cs` | `Program.cs` |

The files remain part of one normal language project. The split exists only to
make the code users usually change obvious.

For .NET, `Program.cs` builds and runs the host:

```csharp
var builder = WebApplication.CreateEmptyBuilder(new WebApplicationOptions
{
    Args = args,
});
builder.WebHost.UseKestrelCore();
builder.Services.AddRouting();
var app = builder.Build();
Application.MapRoutes(app);
app.Urls.Add($"http://0.0.0.0:{Environment.GetEnvironmentVariable("PORT") ?? "8080"}");
await app.RunAsync();
```

`Application.cs` contains ordinary Minimal API code:

```csharp
internal static class Application
{
    public static void MapRoutes(WebApplication app)
    {
        app.Map("/{**path}", (HttpContext context) =>
            Results.Json(new
            {
                message = "hello from .NET",
                path = context.Request.Path.Value ?? "/",
            }));
    }
}
```

Go follows the same shape without another package or service type:

```go
// application.go
func mapApplication(router *gin.Engine) {
    router.GET("/*path", func(context *gin.Context) {
        context.JSON(http.StatusOK, gin.H{
            "message": "hello from Go",
            "path": context.Request.URL.Path,
        })
    })
}
```

```go
// main.go
router := gin.New()
mapApplication(router)
log.Fatal(router.Run(":" + port))
```

Rust keeps framework routes separate from the listening loop:

```rust
// src/application.rs
pub fn routes() -> axum::Router {
    axum::Router::new().fallback(|uri: axum::http::Uri| async move {
        axum::Json(serde_json::json!({
            "message": "hello from Rust",
            "path": uri.path(),
        }))
    })
}
```

```rust
// src/main.rs
let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
axum::serve(listener, application::routes()).await?;
```

Wasm remains in [#2](https://github.com/hyperlight-dev/lampyr/issues/2).
gRPC requires a separate interface design.

## Documentation examples

Keep paired generated snapshots:

```text
docs/examples/hello-nodejs/
docs/examples/hello-nodejs-standalone/
docs/examples/hello-python/
docs/examples/hello-python-standalone/
...
```

The ordinary `hello-<runtime>` examples demonstrate Handler Applications.
The `hello-<runtime>-standalone` examples demonstrate customer-owned listening
processes. CI regenerates both sets from canonical templates and fails on
drift.

## Build and lifecycle

The existing lifecycle remains the contract:

1. build the customer-owned root filesystem from `Dockerfile.rootfs`;
2. snapshot the configured command with the installed Hyperlight runtime;
3. package and verify the standard Lampyr snapshot artifact;
4. deploy and route the revision;
5. activate, scale, inspect status/logs/audit, and delete it through the same
   commands as a Handler Application.

Diagnostics should identify the Application Model and report startup,
listening, and activation failures without claiming to support or upgrade the
customer's server implementation.

Template tests should prove only the maintained contract: the project builds,
the command listens on `$PORT`, one JSON route responds, and the resulting
service completes the ordinary Lampyr lifecycle. Additional framework features
may be documented by their upstream projects but are not Lampyr template
requirements.

## Implementation sequence

1. **Add the application model**
   - Add manifest types and validation for `handler` and `standalone`.
   - Move Handler commands into adapter metadata.
   - Preserve standalone customer commands in artifact metadata.

2. **Split canonical templates**
   - Create `handler` and `standalone` template families.
   - Reclassify current server-owning examples as initial standalone sources
     where practical.
   - Separate implementation and server/bootstrap files.

3. **Add standalone scaffolding**
   - Add the `--standalone` flag.
   - Generate only customer-owned project files.
   - Refuse adapter upgrade operations for standalone projects.

4. **Update builds and diagnostics**
   - Select adapter-owned or customer-owned rootfs build paths by model.
   - Include the selected model and effective command in artifact metadata.
   - Report the model in `ly doctor`, build output, status, and audit metadata
     where useful.

5. **Regenerate paired examples and prove parity**
   - Cover Node.js, Python, Go, Rust, and .NET.
   - Run both models through local development, snapshot build, Kind deploy,
     HTTP, streaming, WebSocket where implemented, events, and lifecycle
     cleanup.

## Related work

- [Runtime Adapters](runtime-adapters.md): generated
  Handler Applications and Runtime Adapters.
- [#6](https://github.com/hyperlight-dev/lampyr/issues/6): future Hyperlight
  host-function model.
- [#2](https://github.com/hyperlight-dev/lampyr/issues/2): Wasm/WASI-HTTP.
