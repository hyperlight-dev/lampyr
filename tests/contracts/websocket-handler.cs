using Lampyr;
using System.Net.WebSockets;

public sealed class Handler : IHandler, IWebSocketHandler
{
    private int _active;
    private int _sent;

    public ValueTask<Response> HandleAsync(Request request, Context context) =>
        ValueTask.FromResult(request.Path == "/state"
            ? Response.Json(new { active = Volatile.Read(ref _active), sent = Volatile.Read(ref _sent) })
            : Response.Text("available"));

    public async ValueTask HandleWebSocketAsync(WebSocketSession session, ProtocolContext context)
    {
        Interlocked.Increment(ref _active);
        try
        {
            if (context.Request.Path == "/fail")
                throw new InvalidOperationException("WebSocket handler failed");
            if (context.Request.Path == "/idle")
                await Task.Delay(Timeout.Infinite, context.CancellationToken);
            if (context.Request.Path == "/close")
                await session.CloseAsync((WebSocketCloseStatus)4000, "finished");
            if (context.Request.Path == "/oversized")
                await session.ReceiveAsync(context.CancellationToken);
            if (context.Request.Path == "/tick")
            {
                // Ignores its token on purpose: shutdown must still end it quietly.
                while (true)
                {
                    await session.SendAsync("tick");
                    await Task.Delay(20);
                }
            }
            if (context.Request.Path is "/send" or "/send-context")
            {
                var payload = new byte[1024 * 1024];
                while (true)
                {
                    await session.SendAsync(payload, context.Request.Path == "/send-context"
                        ? context.CancellationToken
                        : default);
                    Interlocked.Increment(ref _sent);
                }
            }
        }
        finally
        {
            Interlocked.Decrement(ref _active);
            File.WriteAllText("websocket-finished", "closed");
        }
    }
}
