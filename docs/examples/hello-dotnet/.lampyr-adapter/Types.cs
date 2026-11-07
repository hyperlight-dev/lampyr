using System.Net.WebSockets;
using System.Text;
using System.Text.Json;
using Microsoft.AspNetCore.Http;

namespace Lampyr;

public record RequestHead(
    string Method,
    string Path,
    IHeaderDictionary Headers);

public sealed record Request(
    string Method,
    string Path,
    IHeaderDictionary Headers,
    Stream Body) : RequestHead(Method, Path, Headers);

public sealed record Context(CancellationToken CancellationToken);

public sealed record ProtocolContext(RequestHead Request, CancellationToken CancellationToken);

public sealed record CloudEvent(
    string Id,
    string Type,
    string Source,
    string Subject,
    string Time,
    object? Data,
    int Attempt);

public sealed class Response
{
    public Response(
        Stream? body = null,
        int status = 200,
        IHeaderDictionary? headers = null)
    {
        Body = body;
        Status = status;
        Headers = headers ?? new HeaderDictionary();
    }

    public int Status { get; init; }
    public IHeaderDictionary Headers { get; init; }
    public Stream? Body { get; init; }

    public static Response Bytes(byte[] data) => new(new MemoryStream(data, writable: false));

    public static Response Text(string text) => new(
        new MemoryStream(Encoding.UTF8.GetBytes(text), writable: false),
        headers: new HeaderDictionary { ["Content-Type"] = "text/plain; charset=utf-8" });

    [System.Diagnostics.CodeAnalysis.UnconditionalSuppressMessage(
        "Trimming", "IL2026",
        Justification = "The handler assembly is preserved for JSON response metadata.")]
    public static Response Json(object? value, int status = 200, IHeaderDictionary? headers = null) => new(
        new MemoryStream(JsonSerializer.SerializeToUtf8Bytes(value, JsonSerializerOptions.Web), writable: false),
        status, JsonHeaders(headers));

    private static IHeaderDictionary JsonHeaders(IHeaderDictionary? headers)
    {
        headers ??= new HeaderDictionary();
        if (!headers.ContainsKey("Content-Type")) headers["Content-Type"] = "application/json";
        return headers;
    }
}

public static class Bodies
{
    public const int ReadLimit = 1024 * 1024;

    public static async ValueTask<byte[]> ReadAllAsync(
        Stream stream, CancellationToken cancellationToken = default)
    {
        using var buffer = new MemoryStream();
        var chunk = new byte[16 * 1024];
        while (true)
        {
            var count = await stream.ReadAsync(chunk, cancellationToken);
            if (count == 0)
                return buffer.ToArray();
            if (buffer.Length + count > ReadLimit)
                throw new HandlerError(413, "body exceeds 1 MiB");
            buffer.Write(chunk, 0, count);
        }
    }

    public static async ValueTask<string> TextAsync(Stream stream, CancellationToken cancellationToken = default)
    {
        var data = await ReadAllAsync(stream, cancellationToken);
        try { return new UTF8Encoding(false, true).GetString(data); }
        catch (DecoderFallbackException) { throw new HandlerError(400, "invalid UTF-8 body"); }
    }

    [System.Diagnostics.CodeAnalysis.UnconditionalSuppressMessage(
        "Trimming", "IL2026",
        Justification = "The handler assembly is preserved for JSON request metadata.")]
    public static async ValueTask<T?> JsonAsync<T>(Stream stream, CancellationToken cancellationToken = default)
    {
        try { return JsonSerializer.Deserialize<T>(await TextAsync(stream, cancellationToken), JsonSerializerOptions.Web); }
        catch (JsonException) { throw new HandlerError(400, "invalid JSON body"); }
    }

    public static Stream FromChunks(IAsyncEnumerable<ReadOnlyMemory<byte>> chunks) => new ChunkStream(chunks);

    private sealed class ChunkStream(IAsyncEnumerable<ReadOnlyMemory<byte>> chunks) : Stream
    {
        private IAsyncEnumerator<ReadOnlyMemory<byte>>? _enumerator;
        private ReadOnlyMemory<byte> _remaining;
        private bool _disposed;

        public override async ValueTask<int> ReadAsync(
            Memory<byte> buffer, CancellationToken cancellationToken = default)
        {
            ObjectDisposedException.ThrowIf(_disposed, this);
            if (buffer.Length == 0) return 0;
            _enumerator ??= chunks.GetAsyncEnumerator(cancellationToken);
            while (_remaining.IsEmpty)
            {
                if (!await _enumerator.MoveNextAsync()) return 0;
                _remaining = _enumerator.Current;
            }
            var count = Math.Min(buffer.Length, _remaining.Length);
            _remaining[..count].CopyTo(buffer);
            _remaining = _remaining[count..];
            return count;
        }

        public override async ValueTask DisposeAsync()
        {
            if (_disposed) return;
            _disposed = true;
            if (_enumerator is not null) await _enumerator.DisposeAsync();
            _enumerator = null;
            GC.SuppressFinalize(this);
        }

        protected override void Dispose(bool disposing)
        {
            if (disposing) DisposeAsync().AsTask().GetAwaiter().GetResult();
            base.Dispose(disposing);
        }

        public override Task<int> ReadAsync(byte[] buffer, int offset, int count, CancellationToken cancellationToken) =>
            ReadAsync(buffer.AsMemory(offset, count), cancellationToken).AsTask();

        public override bool CanRead => !_disposed;
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
}

public sealed class HandlerError(int status, string message) : Exception(message)
{
    public int Status { get; } = status is >= 400 and <= 599
        ? status
        : throw new ArgumentOutOfRangeException(nameof(status), "Handler errors require status 400 through 599.");
}

public interface IHandler
{
    ValueTask<Response> HandleAsync(Request request, Context context);
}

public interface IEventHandler
{
    ValueTask<Response> HandleEventAsync(CloudEvent cloudEvent, ProtocolContext context);
}

public interface IWebSocketHandler
{
    ValueTask HandleWebSocketAsync(WebSocketSession session, ProtocolContext context);
}

public sealed class WebSocketSession
{
    private const int MessageLimit = 1024 * 1024;
    private const int QueueLimit = 16;
    private readonly WebSocket _socket;
    private readonly System.Threading.Channels.Channel<object> _messages =
        System.Threading.Channels.Channel.CreateBounded<object>(new System.Threading.Channels.BoundedChannelOptions(QueueLimit)
        {
            SingleReader = true,
            SingleWriter = true,
        });
    private readonly CancellationTokenSource _lifetime;
    private readonly CancellationToken _invocation;
    private readonly CancellationToken _shutdown;
    private readonly SemaphoreSlim _sendLock = new(1, 1);
    private readonly CancellationToken _hook;
    private readonly Task _reader;

    internal WebSocketSession(
        WebSocket socket,
        CancellationTokenSource disconnected,
        CancellationToken hook,
        CancellationToken shutdown,
        CancellationToken requestAborted)
    {
        _socket = socket;
        _lifetime = CancellationTokenSource.CreateLinkedTokenSource(requestAborted);
        _invocation = disconnected.Token;
        _shutdown = shutdown;
        _hook = hook;
        _reader = ReadAsync(disconnected, _lifetime.Token);
    }

    // The adapter owns the socket's read side so a peer close reaches idle hooks.
    private async Task ReadAsync(CancellationTokenSource disconnected, CancellationToken shutdown)
    {
        try
        {
            while (true)
            {
                var message = await ReceiveFrameAsync(shutdown);
                if (message is null)
                {
                    _messages.Writer.TryComplete();
                    return;
                }
                if (!_messages.Writer.TryWrite(message))
                {
                    var error = new WebSocketException("WebSocket receive queue overloaded.");
                    Console.Error.WriteLine($"WebSocket receive failed: {error}");
                    await CloseOutputAsync((WebSocketCloseStatus)1013, "overloaded", shutdown);
                    _messages.Writer.TryComplete(error);
                    return;
                }
            }
        }
        catch (OperationCanceledException) when (shutdown.IsCancellationRequested)
        {
            _messages.Writer.TryComplete();
        }
        catch (Exception error) when (error is WebSocketException or OperationCanceledException or DecoderFallbackException)
        {
            if (error is OperationCanceledException)
                _socket.Abort();
            Console.Error.WriteLine($"WebSocket receive failed: {error}");
            _messages.Writer.TryComplete(error);
        }
        finally
        {
            _messages.Writer.TryComplete();
            disconnected.Cancel();
        }
    }

    public async ValueTask<object?> ReceiveAsync(CancellationToken cancellationToken = default)
    {
        using var linked = CancellationTokenSource.CreateLinkedTokenSource(_invocation, cancellationToken);
        try
        {
            return await _messages.Reader.ReadAsync(linked.Token);
        }
        catch (System.Threading.Channels.ChannelClosedException error) when (error.InnerException is null)
        {
            return null;
        }
    }

    private async Task<object?> ReceiveFrameAsync(CancellationToken cancellationToken)
    {
        var buffer = new byte[16 * 1024];
        using var message = new MemoryStream();
        WebSocketMessageType? messageType = null;
        while (true)
        {
            var result = await _socket.ReceiveAsync(buffer, cancellationToken);
            if (result.MessageType == WebSocketMessageType.Close)
                return null;

            messageType ??= result.MessageType;
            if (messageType != result.MessageType)
                throw new WebSocketException("WebSocket message type changed between fragments.");

            if (message.Length + result.Count > MessageLimit)
            {
                await CloseOutputAsync(
                    WebSocketCloseStatus.MessageTooBig, "message too large", cancellationToken);
                throw new WebSocketException("WebSocket message is too large.");
            }

            message.Write(buffer, 0, result.Count);
            if (!result.EndOfMessage)
                continue;

            var payload = message.ToArray();
            return messageType == WebSocketMessageType.Text
                ? new System.Text.UTF8Encoding(false, true).GetString(payload)
                : payload;
        }
    }

    // Bounds each send so a peer that stops reading cannot pin the hook.
    private static readonly TimeSpan SendTimeout = TimeSpan.FromSeconds(30);

    public async ValueTask SendAsync(object message, CancellationToken cancellationToken = default)
    {
        _hook.ThrowIfCancellationRequested();
        _shutdown.ThrowIfCancellationRequested();
        using var linked = CancellationTokenSource.CreateLinkedTokenSource(_invocation, cancellationToken);
        linked.CancelAfter(SendTimeout);
        try
        {
            await _sendLock.WaitAsync(linked.Token);
            try
            {
                _hook.ThrowIfCancellationRequested();
                _shutdown.ThrowIfCancellationRequested();
                await SendMessageAsync(message, linked.Token);
            }
            finally
            {
                _sendLock.Release();
            }
        }
        // A send interrupted by shutdown or disconnect is cancellation, not a socket failure.
        catch (Exception error) when (
            (error is WebSocketException or OperationCanceledException)
            && (_hook.IsCancellationRequested || _shutdown.IsCancellationRequested))
        {
            var token = _shutdown.IsCancellationRequested ? _shutdown : _hook;
            throw new OperationCanceledException(error.Message, error, token);
        }
        catch (OperationCanceledException) when (
            !_invocation.IsCancellationRequested && !cancellationToken.IsCancellationRequested)
        {
            throw new TimeoutException("WebSocket send timed out");
        }
    }

    private async ValueTask SendMessageAsync(object message, CancellationToken cancellationToken)
    {
        switch (message)
        {
            case string text:
                await _socket.SendAsync(
                    System.Text.Encoding.UTF8.GetBytes(text),
                    WebSocketMessageType.Text,
                    endOfMessage: true,
                    cancellationToken);
                break;
            case byte[] bytes:
                await _socket.SendAsync(
                    bytes,
                    WebSocketMessageType.Binary,
                    endOfMessage: true,
                    cancellationToken);
                break;
            case ReadOnlyMemory<byte> bytes:
                await _socket.SendAsync(
                    bytes,
                    WebSocketMessageType.Binary,
                    endOfMessage: true,
                    cancellationToken);
                break;
            default:
                throw new ArgumentException("WebSocket messages must be text or bytes.", nameof(message));
        }
    }

    public async ValueTask CloseAsync(
        WebSocketCloseStatus closeStatus = WebSocketCloseStatus.NormalClosure,
        string? statusDescription = null,
        CancellationToken cancellationToken = default)
    {
        using var timeout = CancellationTokenSource.CreateLinkedTokenSource(_lifetime.Token, cancellationToken);
        timeout.CancelAfter(TimeSpan.FromSeconds(1));
        try
        {
            await CloseOutputAsync(closeStatus, statusDescription, timeout.Token);
            await _reader.WaitAsync(timeout.Token);
        }
        catch (Exception error) when (error is WebSocketException or OperationCanceledException)
        {
            _socket.Abort();
            throw;
        }
    }

    private async ValueTask CloseOutputAsync(
        WebSocketCloseStatus closeStatus, string? statusDescription, CancellationToken cancellationToken)
    {
        using var timeout = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken);
        timeout.CancelAfter(TimeSpan.FromSeconds(1));
        await _sendLock.WaitAsync(timeout.Token);
        try
        {
            if (_socket.State is WebSocketState.Open or WebSocketState.CloseReceived)
                await _socket.CloseOutputAsync(closeStatus, statusDescription, timeout.Token);
        }
        finally
        {
            _sendLock.Release();
        }
    }

    internal async ValueTask StopAsync()
    {
        _lifetime.Cancel();
        try
        {
            await _reader;
        }
        finally
        {
            _lifetime.Dispose();
        }
    }
}
