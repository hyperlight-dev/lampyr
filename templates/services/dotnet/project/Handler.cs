using System.Text.Json;
using System.Diagnostics.CodeAnalysis;
using Lampyr;

public sealed class Handler : IHandler, IEventHandler, IWebSocketHandler
{
    private static readonly JsonSerializerOptions JsonOptions = new(JsonSerializerDefaults.Web);

    public ValueTask<Response> HandleAsync(Request request, Context context)
    {
        if (request.Path == "/stream")
            return ValueTask.FromResult(new Response(
                body: Bodies.FromChunks(StreamChunks()),
                headers: new Microsoft.AspNetCore.Http.HeaderDictionary { ["Content-Type"] = "application/x-ndjson" }));

        return ValueTask.FromResult(Response.Json(
            new { message = "hello from .NET on Hyperlight edge", path = request.Path }));
    }

    [UnconditionalSuppressMessage(
        "Trimming",
        "IL2026",
        Justification = "The adapter preserves the handler assembly and its JSON response types.")]
    public ValueTask<Response> HandleEventAsync(CloudEvent cloudEvent, ProtocolContext context)
    {
        var path = context.Request.Path;
        Console.WriteLine(JsonSerializer.Serialize(new
        {
            timestamp = DateTimeOffset.UtcNow,
            kind = "handler",
            service = "hello-dotnet",
            message = "event received",
            path,
            @event = cloudEvent
        }, JsonOptions));

        if (path == "/events/knative")
            return ValueTask.FromResult(new Response(status: 204));
        if (path == "/events/fail")
            return ValueTask.FromResult(Response.Json(
                new { message = "hello from .NET on Hyperlight edge", path, @event = cloudEvent }, status: 500));
        return ValueTask.FromResult(Response.Json(
            new { message = "hello from .NET on Hyperlight edge", path, @event = cloudEvent }));
    }

    public async ValueTask HandleWebSocketAsync(WebSocketSession session, ProtocolContext context)
    {
        while (await session.ReceiveAsync(context.CancellationToken) is { } message)
            await session.SendAsync(message, context.CancellationToken);
    }

    [UnconditionalSuppressMessage(
        "Trimming",
        "IL2026",
        Justification = "The adapter preserves the handler assembly and its JSON response types.")]
    private static async IAsyncEnumerable<ReadOnlyMemory<byte>> StreamChunks()
    {
        for (var index = 1; index <= 3; index++)
        {
            yield return System.Text.Encoding.UTF8.GetBytes(JsonSerializer.Serialize(new { chunk = index }, JsonOptions) + "\n");
            await Task.Yield();
        }
    }
}
