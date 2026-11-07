import datetime
import json
from dataclasses import asdict

from lampyr_types import Context, Event, ProtocolContext, Request, Response, WebSocketSession


def handle(request: Request, context: Context) -> Response:
    return Response.json({
        "message": "hello from Python on Hyperlight edge",
        "path": request.path,
    })


def handle_event(event: Event, context: ProtocolContext) -> Response:
    path = context.request.path
    print(json.dumps({
        "timestamp": datetime.datetime.now(datetime.timezone.utc).isoformat().replace("+00:00", "Z"),
        "kind": "handler", "service": "hello-python",
        "message": "event received", "path": path, "event": asdict(event),
    }, separators=(",", ":")), flush=True)
    return Response.json(
        {"message": "hello from Python on Hyperlight edge", "path": path, "event": asdict(event)},
        status=500 if path == "/events/fail" else 204 if path == "/events/knative" else 200,
    )


def handle_websocket(session: WebSocketSession, context: ProtocolContext) -> None:
    if context.request.path != "/ws":
        session.close(1008, "not found")
        return
    for message in session:
        session.send(message)
