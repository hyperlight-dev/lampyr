import time
from pathlib import Path
from lampyr_types import Body, Context, HandlerError, Headers, Request, Response

closed_bodies = []


def handle(request: Request, context: Context) -> Response:
    if request.path == "/closed-bodies":
        return Response.json(closed_bodies)
    if request.path in ("/close-head", "/close-204", "/close-304"):
        class Source:
            def __iter__(self):
                return self

            def __next__(self):
                raise RuntimeError("bodyless response source was read")

            def close(self):
                closed_bodies.append(request.path)

        return Response(Body(Source()), status=204 if request.path == "/close-204"
                        else 304 if request.path == "/close-304" else 200)
    if request.path == "/value":
        return Response.json({"ok": True}, status=201, headers=Headers([("x-contract", "value")]))
    if request.path == "/bytes":
        return Response.bytes(b"raw\0bytes")
    if request.path == "/text":
        return Response.text("plain text", headers=Headers([("Content-Type", "text/plain")]))
    if request.path == "/array":
        return Response.json([1, 2, 3])
    if request.path == "/stream":
        return Response(Body([b"one\n", b"two\n", b"three\n"]))
    if request.path == "/typed-error":
        raise HandlerError(422, "<bad>\r\nX-Injected: yes")
    if request.path == "/error":
        raise RuntimeError("private details")
    if request.path == "/serialize-error":
        body = {}
        body["self"] = body
        return Response.json(body)
    if request.path == "/empty":
        return Response.text("must not appear", status=204)
    if request.path == "/headers":
        return Response.text("bad", headers=Headers([("content-length", "999")]))
    if request.path == "/unicode-header":
        return Response.text("bad", headers=Headers([("x-locale", "snowman ☃")]))
    if request.path == "/not-modified":
        return Response.json({"ok": True}, status=304)
    if request.path == "/broken-stream":
        def broken():
            yield b"first"
            raise RuntimeError("stream failed")
        return Response(Body(broken()))
    if request.path == "/upload-stream":
        return Response(request.body)
    if request.path == "/json-body":
        return Response.json(request.body.json())
    if request.path == "/consume-twice":
        request.body.bytes()
        return Response.bytes(request.body.bytes())
    if request.path == "/read-limit":
        return Response.bytes(request.body.bytes(4))
    if request.path == "/http-context":
        return Response.json({"requestInContext": hasattr(context, "request")})
    if request.path == "/own-nosniff":
        return Response.text("own", headers=Headers([("x-content-type-options", "handler")]))
    if request.path == "/repeated-headers":
        return Response.json({"values": request.headers.get_all("x-repeat")},
                             headers=Headers([("set-cookie", "a=1"), ("set-cookie", "b=2")]))
    if request.path == "/backpressure":
        def pressure():
            chunk = b"x" * (1024 * 1024)
            for index in range(1, 129):
                Path("produced").write_text(str(index))
                yield chunk
        return Response(Body(pressure()))
    if request.path == "/cancel":
        def chunks():
            try:
                while not context.cancelled.is_set():
                    yield b"x" * 65536
                    Path("sleeping").write_text("ready")
                    time.sleep(0.25)
            finally:
                Path("cancelled").write_text("closed")
        return Response(Body(chunks()))
    return Response.json({
        "method": request.method, "path": request.path,
        "body": request.body.text(), "header": request.headers.get("x-test"),
    })
