using Lampyr;
using System.Runtime.CompilerServices;
using System.Text;
using Microsoft.AspNetCore.Http;

public sealed class Handler : IHandler
{
    public async ValueTask<Response> HandleAsync(Request request, Context context)
    {
        return request.Path switch
        {
            "/http-context" => Response.Json(new { requestInContext = typeof(Context).GetProperty("Request") is not null }),
            "/upload-stream" => new Response(request.Body),
            "/own-nosniff" => new Response(Response.Text("own").Body, headers: new HeaderDictionary
                { ["Content-Type"] = "text/plain; charset=utf-8", ["X-Content-Type-Options"] = "handler" }),
            "/repeated-headers" => Response.Json(new { values = request.Headers["X-Repeat"].ToArray() },
                headers: new HeaderDictionary { ["Set-Cookie"] = new[] { "a=1", "b=2" } }),
            "/json-body" => Response.Json(await Bodies.JsonAsync<System.Text.Json.JsonElement>(
                request.Body, context.CancellationToken)),
            "/typed-json" when request.Method == "GET" => Response.Json(new MessageBody { Message = "hello", Count = 7 }),
            "/typed-json" => Response.Json(await Bodies.JsonAsync<MessageBody>(
                request.Body, context.CancellationToken)),
            "/value" => Response.Json(new { ok = true }, 201, new HeaderDictionary { ["x-contract"] = "value" }),
            "/bytes" => Response.Bytes(Encoding.UTF8.GetBytes("raw\0bytes")),
            "/text" => new Response(Response.Text("plain text").Body,
                headers: new HeaderDictionary { ["Content-Type"] = "text/plain" }),
            "/array" => Response.Json(new[] { 1, 2, 3 }),
            "/stream" => new Response(Bodies.FromChunks(TextChunks())),
            "/typed-error" => throw new HandlerError(422, "<bad>\r\nX-Injected: yes"),
            "/error" => throw new InvalidOperationException("private details"),
            "/serialize-error" => Response.Json(CircularObject()),
            "/empty" => new Response(Response.Text("must not appear").Body, 204),
            "/headers" => new Response(Response.Text("bad").Body, headers: new HeaderDictionary { ["content-length"] = "999" }),
            "/unicode-header" => new Response(Response.Text("bad").Body, headers: new HeaderDictionary { ["x-locale"] = "snowman \u2603" }),
            "/closed-bodies" => Response.Json(Closed.ToArray()),
            "/close-head" or "/close-204" or "/close-304" => new Response(
                new CloseRecorder(request.Path),
                request.Path == "/close-204" ? 204 : request.Path == "/close-304" ? 304 : 200),
            "/not-modified" => new Response(Response.Text("must not appear").Body, 304),
            "/broken-stream" => new Response(Bodies.FromChunks(BrokenChunks())),
            "/backpressure" => new Response(Bodies.FromChunks(BackpressureChunks())),
            "/cancel" => new Response(Bodies.FromChunks(CancellableChunks(context.CancellationToken))),
            _ => Response.Json(new
            {
                method = request.Method,
                path = request.Path,
                body = await Bodies.TextAsync(request.Body, context.CancellationToken),
                header = request.Headers["x-test"].ToString()
            })
        };
    }

    public sealed class MessageBody
    {
        public string? Message { get; set; }
        public int Count { get; set; }
    }

    private static readonly System.Collections.Concurrent.ConcurrentQueue<string> Closed = new();

    private sealed class CloseRecorder(string path) : Stream
    {
        public override bool CanRead => true;
        public override bool CanSeek => false;
        public override bool CanWrite => false;
        public override long Length => throw new NotSupportedException();
        public override long Position { get => throw new NotSupportedException(); set => throw new NotSupportedException(); }
        public override void Flush() { }
        public override int Read(byte[] buffer, int offset, int count) => throw new InvalidOperationException("bodyless response source was read");
        public override long Seek(long offset, SeekOrigin origin) => throw new NotSupportedException();
        public override void SetLength(long value) => throw new NotSupportedException();
        public override void Write(byte[] buffer, int offset, int count) => throw new NotSupportedException();

        protected override void Dispose(bool disposing)
        {
            if (disposing) Closed.Enqueue(path);
            base.Dispose(disposing);
        }
    }

    private static object CircularObject()
    {
        var value = new Dictionary<string, object?>();
        value["self"] = value;
        return value;
    }

    private static async IAsyncEnumerable<ReadOnlyMemory<byte>> TextChunks()
    {
        yield return Encoding.UTF8.GetBytes("one\n");
        await Task.Yield();
        yield return Encoding.UTF8.GetBytes("two\n");
        await Task.Yield();
        yield return Encoding.UTF8.GetBytes("three\n");
    }

    private static async IAsyncEnumerable<ReadOnlyMemory<byte>> BrokenChunks()
    {
        yield return Encoding.UTF8.GetBytes("first");
        await Task.Yield();
        throw new InvalidOperationException("stream failed");
    }

    private static async IAsyncEnumerable<ReadOnlyMemory<byte>> BackpressureChunks()
    {
        var chunk = new byte[1024 * 1024];
        for (var index = 1; index <= 128; index++)
        {
            await File.WriteAllTextAsync("produced", index.ToString());
            yield return chunk;
        }
    }

    private static async IAsyncEnumerable<ReadOnlyMemory<byte>> CancellableChunks(
        [EnumeratorCancellation] CancellationToken cancellationToken)
    {
        try
        {
            while (true)
            {
                cancellationToken.ThrowIfCancellationRequested();
                yield return Encoding.UTF8.GetBytes(new string('x', 65536));
                await Task.Delay(5, cancellationToken);
            }
        }
        finally
        {
            await File.WriteAllTextAsync("cancelled", "closed");
        }
    }
}
