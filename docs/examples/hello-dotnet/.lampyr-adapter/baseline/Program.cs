using System.Globalization;
using System.Net.WebSockets;
using System.Text;
using System.Text.Json;
using Lampyr;
using Microsoft.AspNetCore.Http.Features;

internal static class Program
{
    private const int RequestBodyLimit = 1024 * 1024;
    private static readonly JsonSerializerOptions JsonOptions = new(JsonSerializerDefaults.Web);

    private static async Task<int> Main(string[] args)
    {
        IHandler handler = new global::Handler();
        var requireEvent = args.Contains("--require-event", StringComparer.Ordinal);
        if (requireEvent && handler is not IEventHandler)
            throw new InvalidOperationException("events policy requires IEventHandler.HandleEventAsync");
        if (args.Contains("--check", StringComparer.Ordinal) || requireEvent)
            return 0;

        var port = int.TryParse(Environment.GetEnvironmentVariable("PORT"), out var configuredPort)
            ? configuredPort
            : 8080;

        var builder = WebApplication.CreateEmptyBuilder(new WebApplicationOptions());
        // Avoid the guest socket transport's byte-consuming zero-length readiness receive.
        builder.WebHost.UseSockets(options => options.WaitForDataBeforeAllocatingBuffer = false);
        builder.WebHost.UseKestrel();
        var app = builder.Build();
        app.UseWebSockets();
        // Browsers must not sniff handler or error bytes into HTML; a handler may
        // set its own X-Content-Type-Options. OnStarting also covers error replies.
        app.Use(async (context, next) =>
        {
            context.Response.OnStarting(() =>
            {
                if (!context.Response.Headers.ContainsKey("X-Content-Type-Options"))
                    context.Response.Headers["X-Content-Type-Options"] = "nosniff";
                return Task.CompletedTask;
            });
            await next(context);
        });
        app.Run(context => HandleRequestAsync(
            context,
            handler,
            app.Services.GetRequiredService<IHostApplicationLifetime>()));

        var host = Environment.GetEnvironmentVariable("LAMPYR_LISTEN_HOST") ?? "0.0.0.0";
        app.Urls.Add($"http://{host}:{port}");
        await app.RunAsync();
        return 0;
    }

    private static async Task HandleRequestAsync(
        HttpContext http,
        IHandler handler,
        IHostApplicationLifetime lifetime)
    {
        using var cancellation = CancellationTokenSource.CreateLinkedTokenSource(
            http.RequestAborted,
            lifetime.ApplicationStopping);

        try
        {
            if (http.Request.ContentLength is > RequestBodyLimit)
                throw new HandlerError(413, "request body too large");
            using var body = new IncomingBody(http.Request.Body, cancellation.Token);
            var request = new Request(
                http.Request.Method,
                RequestPath(http),
                new HeaderDictionary(http.Request.Headers.ToDictionary(
                    header => header.Key, header => header.Value, StringComparer.OrdinalIgnoreCase)),
                body);
            var context = new Context(cancellation.Token);
            var head = new RequestHead(request.Method, request.Path, request.Headers);

            if (request.Method.Equals("GET", StringComparison.Ordinal)
                && http.WebSockets.IsWebSocketRequest)
            {
                if (handler is not IWebSocketHandler webSocketHandler)
                    throw new HandlerError(StatusCodes.Status404NotFound, "WebSocket hook is not supported");

                using var socket = await http.WebSockets.AcceptWebSocketAsync();
                using var socketCancellation = new CancellationTokenSource();
                using var requestAbortedRegistration = http.RequestAborted.Register(() =>
                {
                    if (!lifetime.ApplicationStopping.IsCancellationRequested)
                        socketCancellation.Cancel();
                });
                using var sessionCancellation = new CancellationTokenSource();
                var session = new WebSocketSession(
                    socket, sessionCancellation, sessionCancellation.Token,
                    lifetime.ApplicationStopping, socketCancellation.Token);
                var stopping = Task.Delay(Timeout.Infinite, cancellation.Token);
                try
                {
                    var hook = webSocketHandler.HandleWebSocketAsync(
                        session, new ProtocolContext(head, sessionCancellation.Token)).AsTask();
                    await Task.WhenAny(hook, stopping);
                    if (lifetime.ApplicationStopping.IsCancellationRequested)
                    {
                        await CloseWebSocketForShutdownAsync(http, session);
                        try
                        {
                            await hook;
                        }
                        catch (OperationCanceledException) when (sessionCancellation.IsCancellationRequested)
                        {
                        }
                    }
                    else
                    {
                        await hook;
                        await CloseWebSocketAsync(
                            http, session, WebSocketCloseStatus.NormalClosure, null, socketCancellation.Token);
                    }
                }
                catch (OperationCanceledException) when (lifetime.ApplicationStopping.IsCancellationRequested)
                {
                    await CloseWebSocketForShutdownAsync(http, session);
                }
                catch (OperationCanceledException) when (http.RequestAborted.IsCancellationRequested)
                {
                    http.Abort();
                }
                catch (OperationCanceledException) when (sessionCancellation.IsCancellationRequested)
                {
                    await CloseWebSocketAsync(
                        http, session, WebSocketCloseStatus.NormalClosure, null, cancellation.Token);
                }
                catch (Exception error)
                {
                    LogUnexpected(error);
                    if (socket.State is WebSocketState.Open or WebSocketState.CloseReceived)
                    {
                        await CloseWebSocketAsync(
                            http, session, WebSocketCloseStatus.InternalServerError,
                            "request failed", cancellation.Token);
                    }
                }
                finally
                {
                    await session.StopAsync();
                }
                return;
            }

            // Repeated Content-Type fields are joined in ContentType, so check each
            // field; a duplicated one must reach ParseEvent's duplicate rejection.
            var isStructuredEvent = http.Request.Headers.ContentType
                .Any(value => MediaType(value) == "application/cloudevents+json");
            var isBinaryEvent = request.Headers.Keys.Any(key => key.StartsWith("ce-", StringComparison.OrdinalIgnoreCase));
            Response response;
            if (isStructuredEvent || isBinaryEvent)
            {
                if (handler is not IEventHandler eventHandler)
                    throw new HandlerError(StatusCodes.Status404NotFound, "event hook is not supported");
                var cloudEvent = ParseEvent(request,
                    await Bodies.ReadAllAsync(body, cancellation.Token), isStructuredEvent);
                response = await eventHandler.HandleEventAsync(cloudEvent, new ProtocolContext(head, cancellation.Token));
            }
            else
            {
                response = await handler.HandleAsync(request, context);
            }

            await WriteResponseAsync(http, request, response, cancellation.Token);
        }
        catch (OperationCanceledException) when (cancellation.IsCancellationRequested)
        {
            http.Abort();
        }
        catch (Exception error)
        {
            if (http.Response.HasStarted || http.RequestAborted.IsCancellationRequested)
            {
                if (!http.RequestAborted.IsCancellationRequested)
                    LogUnexpected(error);
                http.Abort();
                return;
            }

            var status = error is HandlerError typedError
                ? typedError.Status
                : StatusCodes.Status500InternalServerError;
            if (error is not HandlerError)
                LogUnexpected(error);
            await WriteErrorAsync(http, status, error is HandlerError ? error.Message : "request failed");
        }
        finally
        {
            cancellation.Cancel();
        }
    }

    // An absolute-form target must not leak scheme and authority into Path,
    // where handler prefix checks would fail open.
    private static string RequestPath(HttpContext http)
    {
        var raw = http.Features.Get<IHttpRequestFeature>()?.RawTarget;
        if (string.IsNullOrEmpty(raw))
            return $"{http.Request.Path}{http.Request.QueryString}";
        if (raw[0] != '/' && raw != "*" && Uri.TryCreate(raw, UriKind.Absolute, out var uri))
            return uri.PathAndQuery;
        return raw;
    }

    private static async Task CloseWebSocketAsync(
        HttpContext http,
        WebSocketSession session,
        WebSocketCloseStatus status,
        string? description,
        CancellationToken cancellationToken)
    {
        try
        {
            await session.CloseAsync(status, description, cancellationToken);
        }
        catch (Exception error) when (error is WebSocketException or OperationCanceledException)
        {
            if (!cancellationToken.IsCancellationRequested)
                LogUnexpected(error);
            http.Abort();
        }
    }

    private static async Task CloseWebSocketForShutdownAsync(HttpContext http, WebSocketSession session)
    {
        try
        {
            await session.CloseAsync((WebSocketCloseStatus)1001, "server shutdown");
        }
        catch (OperationCanceledException error)
        {
            Console.Error.WriteLine($"WebSocket shutdown close did not complete: {error.Message}");
            http.Abort();
        }
        catch (WebSocketException error)
        {
            LogUnexpected(error);
            http.Abort();
        }
    }

    private sealed class IncomingBody(Stream source, CancellationToken invocation) : Stream
    {
        private long _read;
        private bool _disposed;
        public override async ValueTask<int> ReadAsync(
            Memory<byte> buffer, CancellationToken cancellationToken = default)
        {
            ObjectDisposedException.ThrowIf(_disposed, this);
            using var linked = CancellationTokenSource.CreateLinkedTokenSource(invocation, cancellationToken);
            var count = await source.ReadAsync(buffer, linked.Token);
            _read += count;
            if (_read > RequestBodyLimit)
                throw new HandlerError(413, "request body too large");
            return count;
        }
        protected override void Dispose(bool disposing)
        {
            _disposed = true;
            base.Dispose(disposing);
        }
        public override bool CanRead => !_disposed;
        public override Task<int> ReadAsync(byte[] buffer, int offset, int count, CancellationToken cancellationToken) =>
            ReadAsync(buffer.AsMemory(offset, count), cancellationToken).AsTask();
        public override bool CanSeek => false;
        public override bool CanWrite => false;
        public override long Length => throw new NotSupportedException();
        public override long Position { get => throw new NotSupportedException(); set => throw new NotSupportedException(); }
        public override int Read(byte[] buffer, int offset, int count) => throw new NotSupportedException("Use ReadAsync.");
        public override void Flush() { }
        public override long Seek(long offset, SeekOrigin origin) => throw new NotSupportedException();
        public override void SetLength(long value) => throw new NotSupportedException();
        public override void Write(byte[] buffer, int offset, int count) => throw new NotSupportedException();
    }

    private static CloudEvent ParseEvent(RequestHead request, byte[] body, bool structured)
    {
        try
        {
            var text = new UTF8Encoding(false, true).GetString(body);
            var attempt = 1;
            foreach (var (name, values) in request.Headers)
                if ((name.StartsWith("ce-", StringComparison.OrdinalIgnoreCase)
                    || name.Equals("content-type", StringComparison.OrdinalIgnoreCase)
                    || name.Equals("x-lampyr-delivery-attempt", StringComparison.OrdinalIgnoreCase))
                    && values.Count != 1)
                    throw new FormatException("duplicate event header");
            if (request.Headers.TryGetValue("x-lampyr-delivery-attempt", out var rawAttempt)
                && (!int.TryParse(rawAttempt.ToString(), NumberStyles.None, CultureInfo.InvariantCulture, out attempt)
                    || attempt <= 0))
                throw new FormatException("invalid delivery attempt");

            if (structured)
            {
                using var document = JsonDocument.Parse(text);
                var root = document.RootElement;
                if (root.ValueKind != JsonValueKind.Object
                    || RequiredString(root, "specversion") != "1.0")
                    throw new FormatException("invalid CloudEvent version");
                if (root.TryGetProperty("data_base64", out _))
                    throw new FormatException("data_base64 is not supported");

                var id = RequiredString(root, "id");
                var type = RequiredString(root, "type");
                var source = RequiredString(root, "source");
                var subject = OptionalString(root, "subject");
                var time = OptionalString(root, "time");
                object? data = root.TryGetProperty("data", out var value) ? value.Clone() : null;
                return new CloudEvent(id, type, source, subject, time, data, attempt);
            }

            if (!request.Headers.TryGetValue("ce-specversion", out var specVersion) || specVersion != "1.0")
                throw new FormatException("invalid CloudEvent version");
            var eventId = RequiredHeader(request, "ce-id");
            var eventType = RequiredHeader(request, "ce-type");
            var eventSource = RequiredHeader(request, "ce-source");
            var eventSubject = request.Headers["ce-subject"].ToString();
            var eventTime = request.Headers["ce-time"].ToString();
            object? eventData = text;
            try
            {
                using var dataDocument = JsonDocument.Parse(text);
                eventData = dataDocument.RootElement.Clone();
            }
            catch (JsonException)
            {
            }
            return new CloudEvent(eventId, eventType, eventSource, eventSubject, eventTime, eventData, attempt);
        }
        catch (Exception error) when (error is JsonException or FormatException or DecoderFallbackException)
        {
            throw new HandlerError(StatusCodes.Status400BadRequest, "invalid CloudEvent");
        }
    }

    private static string RequiredString(JsonElement element, string property)
    {
        if (!element.TryGetProperty(property, out var value)
            || value.ValueKind != JsonValueKind.String
            || string.IsNullOrEmpty(value.GetString()))
            throw new FormatException("missing CloudEvent attribute");
        return value.GetString()!;
    }

    private static string OptionalString(JsonElement element, string property)
    {
        if (!element.TryGetProperty(property, out var value))
            return "";
        if (value.ValueKind != JsonValueKind.String)
            throw new FormatException("invalid CloudEvent attribute");
        return value.GetString() ?? "";
    }

    private static string RequiredHeader(RequestHead request, string header)
    {
        if (!request.Headers.TryGetValue(header, out var value) || string.IsNullOrEmpty(value))
            throw new FormatException("missing CloudEvent attribute");
        return value.ToString();
    }

    private static string MediaType(string? contentType) =>
        contentType?.Split(';', 2)[0].Trim().ToLowerInvariant() ?? "";

    private static async Task WriteResponseAsync(
        HttpContext http,
        Request request,
        Response response,
        CancellationToken cancellationToken)
    {
        await using var body = response.Body;
        if (response.Status is < 200 or > 599)
            throw new InvalidOperationException("handler response status must be between 200 and 599");

        foreach (var (name, values) in response.Headers)
        {
            if (name.Equals("content-length", StringComparison.OrdinalIgnoreCase)
                || name.Equals("transfer-encoding", StringComparison.OrdinalIgnoreCase)
                || name.Equals("connection", StringComparison.OrdinalIgnoreCase))
                throw new InvalidOperationException("response framing headers are adapter-owned");
            if (!IsValidHeaderName(name) || values.Any(value => value is null
                || value.Any(character => character != '\t' && (character < ' ' || character > '~'))))
                throw new InvalidOperationException("invalid response header");
        }

        var noBody = request.Method.Equals("HEAD", StringComparison.Ordinal)
            || response.Status is StatusCodes.Status204NoContent or StatusCodes.Status304NotModified;
        http.Response.StatusCode = response.Status;
        foreach (var (name, value) in response.Headers)
            http.Response.Headers[name] = value;

        if (noBody || body is null)
            return;
        await http.Response.StartAsync(cancellationToken);
        var buffer = new byte[32 * 1024];
        while (true)
        {
            var count = await body.ReadAsync(buffer, cancellationToken);
            if (count == 0) break;
            await http.Response.Body.WriteAsync(buffer.AsMemory(0, count), cancellationToken);
            await http.Response.Body.FlushAsync(cancellationToken);
        }
    }

    private static bool IsValidHeaderName(string name) =>
        name.Length > 0 && name.All(character =>
            char.IsAsciiLetterOrDigit(character) || "!#$%&'*+-.^_`|~".Contains(character));


    [System.Diagnostics.CodeAnalysis.UnconditionalSuppressMessage(
        "Trimming",
        "IL2026",
        Justification = "The handler assembly is preserved so handler-owned response types retain their JSON metadata.")]
    private static async Task WriteErrorAsync(HttpContext http, int status, string message)
    {
        http.Response.Clear();
        http.Response.StatusCode = status;
        http.Response.ContentType = "application/json";
        var body = JsonSerializer.SerializeToUtf8Bytes(new { error = message }, JsonOptions);
        http.Response.ContentLength = body.Length;
        await http.Response.Body.WriteAsync(body, http.RequestAborted);
    }

    private static void LogUnexpected(Exception error) =>
        Console.Error.WriteLine($"request failed: {error}");

}
