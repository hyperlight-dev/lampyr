import asyncio
from concurrent.futures import ThreadPoolExecutor, TimeoutError as FutureTimeoutError
from functools import partial
import html
import importlib
import inspect
import json
import os
from pathlib import Path
import sys
import threading
import traceback

sys.path.insert(0, str(Path(__file__).resolve().parent / "site-packages"))
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
from aiohttp import WSMsgType, web
from lampyr_types import Body, Context, Event, HandlerError, Headers, ProtocolContext, Request, RequestHead, Response

hooks = importlib.import_module("handler")
if not callable(getattr(hooks, "handle", None)):
    raise TypeError("required handle(request, context) hook is missing")
for name in ("handle", "handle_event", "handle_websocket"):
    if hasattr(hooks, name) and not callable(getattr(hooks, name)):
        raise TypeError(f"{name} must be callable")
    if hasattr(hooks, name):
        hook = getattr(hooks, name)
        if inspect.iscoroutinefunction(hook):
            raise TypeError(f"{name} must be synchronous")
        inspect.signature(hook).bind(object(), object())
if ("--require-event" in sys.argv or os.environ.get("LAMPYR_REQUIRE_EVENT") == "1") and not callable(
        getattr(hooks, "handle_event", None)):
    raise TypeError("events policy requires handle_event")
if "--check" in sys.argv:
    sys.exit(0)


def media_type(headers):
    return headers.get("content-type", "").split(";")[0].strip().lower()


def event_of(request):
    headers = request.headers
    try:
        raw_attempt = headers.get("x-lampyr-delivery-attempt", "1")
        if not raw_attempt.isascii() or not raw_attempt.isdecimal():
            raise ValueError("invalid delivery attempt")
        attempt = int(raw_attempt)
        if not 1 <= attempt <= 9007199254740991:
            raise ValueError("invalid delivery attempt")
        if media_type(headers) == "application/cloudevents+json":
            value = request.body.json()
            if not isinstance(value, dict) or value.get("specversion") != "1.0" or not all(
                    isinstance(value.get(key), str) and value[key] for key in ("id", "type", "source")):
                raise ValueError("invalid CloudEvent version")
            if any(key in value and not isinstance(value[key], str) for key in ("subject", "time")):
                raise ValueError("invalid CloudEvent attributes")
            if "data_base64" in value:
                raise ValueError("unsupported CloudEvent encoding")
            return Event(id=value["id"], type=value["type"], source=value["source"],
                         subject=value.get("subject", ""), time=value.get("time", ""),
                         data=value.get("data"), attempt=attempt)
        if headers.get("ce-specversion") != "1.0":
            raise ValueError("invalid CloudEvent version")
        if not all(headers.get(key) for key in ("ce-id", "ce-type", "ce-source")):
            raise ValueError("missing CloudEvent attributes")
        data = request.body.text()
        try:
            data = json.loads(data)
        except json.JSONDecodeError:
            pass
        return Event(id=headers.get("ce-id"), type=headers.get("ce-type"), source=headers.get("ce-source"),
                     subject=headers.get("ce-subject", ""), time=headers.get("ce-time", ""),
                     data=data, attempt=attempt)
    except (ValueError, KeyError, TypeError) as error:
        raise HandlerError(400, "invalid CloudEvent") from error


async def call_sync(cancelled, function, *args, executor=None):
    # Let an in-flight hook or next() finish before closing its iterator.
    task = asyncio.get_running_loop().run_in_executor(executor, partial(function, *args))
    try:
        return await asyncio.shield(task)
    except asyncio.CancelledError:
        cancelled.set()
        await task
        raise


class WebSocket:
    def __init__(self, socket, loop):
        self.socket = socket
        self.loop = loop

    def send(self, data):
        operation = self.socket.send_str(data) if isinstance(data, str) else self.socket.send_bytes(data)
        asyncio.run_coroutine_threadsafe(operation, self.loop).result()

    def close(self, code=1000, reason=""):
        asyncio.run_coroutine_threadsafe(
            self.socket.close(code=code, message=reason.encode()), self.loop).result()

    def __iter__(self):
        while True:
            message = asyncio.run_coroutine_threadsafe(self.socket.receive(), self.loop).result()
            if message.type in (WSMsgType.TEXT, WSMsgType.BINARY):
                yield message.data
            elif message.type == WSMsgType.ERROR:
                raise self.socket.exception()
            else:
                return


active = set()
websocket_workers = ThreadPoolExecutor(max_workers=8, thread_name_prefix="websocket")
websocket_slots = asyncio.Semaphore(8)
websockets = set()


async def dispatch(incoming):
    cancelled = threading.Event()
    active.add(cancelled)
    outgoing = None
    iterator = None
    request = None
    response = None
    try:
        if incoming.content_length is not None and incoming.content_length > 1024 * 1024:
            raise web.HTTPRequestEntityTooLarge(max_size=1024 * 1024, actual_size=incoming.content_length)
        loop = asyncio.get_running_loop()

        def chunks():
            while True:
                pending = asyncio.run_coroutine_threadsafe(incoming.content.read(65536), loop)
                while True:
                    try:
                        chunk = pending.result(timeout=0.1)
                        break
                    except FutureTimeoutError:
                        if cancelled.is_set():
                            pending.cancel()
                            raise ConnectionAbortedError("request body read cancelled")
                if not chunk:
                    return
                yield chunk

        headers = Headers((name.decode("ascii"), value.decode("latin-1"))
                          for name, value in incoming.raw_headers)
        head = RequestHead(incoming.method, incoming.raw_path, headers)
        request = Request(head.method, head.path, head.headers, Body(chunks(), limit=1024 * 1024))
        context = Context(cancelled)
        if incoming.headers.get("upgrade", "").lower() == "websocket":
            hook = getattr(hooks, "handle_websocket", None)
            if hook is None:
                return web.Response(status=404)
            if websocket_slots.locked():
                raise web.HTTPServiceUnavailable(text="WebSocket capacity reached")
            async with websocket_slots:
                outgoing = web.WebSocketResponse(max_msg_size=1024 * 1024, compress=False)
                await outgoing.prepare(incoming)
                websockets.add(outgoing)
                try:
                    await call_sync(cancelled, hook, WebSocket(outgoing, asyncio.get_running_loop()),
                                    ProtocolContext(cancelled, head), executor=websocket_workers)
                    await outgoing.close()
                    return outgoing
                finally:
                    websockets.discard(outgoing)
        is_event = any(key.startswith("ce-") for key, _ in request.headers) or media_type(
            request.headers) == "application/cloudevents+json"
        if is_event:
            hook = getattr(hooks, "handle_event", None)
            if hook:
                event = await call_sync(cancelled, event_of, request)
                response = await call_sync(cancelled, hook, event, ProtocolContext(cancelled, head))
            else:
                response = Response(status=404)
        else:
            response = await call_sync(cancelled, hooks.handle, request, context)
        if (not isinstance(response, Response) or not isinstance(response.body, Body)
                or not isinstance(response.headers, Headers) or not isinstance(response.status, int)
                or isinstance(response.status, bool) or not 200 <= response.status <= 599):
            raise TypeError("hook must return a Response with a valid status")
        headers = []
        for key, value in response.headers:
            if key.lower() in ("content-length", "transfer-encoding", "connection"):
                raise TypeError("framing headers are adapter-owned")
            key.encode("ascii")
            value.encode("latin-1")
            if "\r" in key + value or "\n" in key + value:
                raise TypeError("invalid response header")
            headers.append((key, value))
        outgoing = web.StreamResponse(status=response.status, headers=headers)
        await outgoing.prepare(incoming)
        if incoming.method != "HEAD" and response.status not in (204, 304):
            iterator = iter(response.body)
            end = object()
            while True:
                if cancelled.is_set():
                    raise asyncio.CancelledError
                chunk = await call_sync(cancelled, next, iterator, end)
                if chunk is end:
                    break
                await outgoing.write(chunk)
        await outgoing.write_eof()
        return outgoing
    except web.HTTPException:
        raise
    except Exception as error:
        traceback.print_exc()
        if outgoing is not None and outgoing.prepared:
            raise
        if isinstance(error, HandlerError):
            return web.Response(status=error.status, text=html.escape(str(error)), content_type="text/html")
        return web.Response(status=500, text="request failed")
    finally:
        cancelled.set()
        if iterator is not None and hasattr(iterator, "close"):
            await call_sync(cancelled, iterator.close)
        if isinstance(response, Response) and isinstance(response.body, Body):
            await call_sync(cancelled, response.body.close)
        if request is not None:
            await call_sync(cancelled, request.body.close)
        active.discard(cancelled)


async def shutdown(app):
    for cancelled in active:
        cancelled.set()
    await asyncio.gather(*(socket.close(code=1001) for socket in list(websockets)))


async def nosniff(_request, response):
    # Browsers must not sniff handler or error bytes into HTML; a handler may
    # set its own X-Content-Type-Options.
    if not isinstance(response, web.WebSocketResponse):
        response.headers.setdefault("X-Content-Type-Options", "nosniff")


app = web.Application(client_max_size=1024 * 1024, handler_args={"auto_decompress": False})
app.on_response_prepare.append(nosniff)
app.router.add_route("*", "/{path:.*}", dispatch)
app.on_shutdown.append(shutdown)
try:
    web.run_app(app, host=os.environ.get("LAMPYR_LISTEN_HOST", "0.0.0.0"), port=int(os.environ.get("PORT", "8080")),
                handler_cancellation=True, shutdown_timeout=3)
finally:
    websocket_workers.shutdown(wait=True, cancel_futures=True)
