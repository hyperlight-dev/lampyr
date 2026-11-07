"""What a fiber knows about itself, for the helper's KVM tests: its
incarnation and payload from /run/fiberd, and whether that mount can be
written. `?dirty=N` first writes N bytes the fiber keeps, so the tests can
watch its W grow. `?spin=1` waits for a body the client holds back and then
spins, so a park taken while it waits spins in its resume, once the
connection is gone."""

import json
from urllib.parse import parse_qs, urlsplit

from lampyr_types import Context, Event, ProtocolContext, Request, Response

MOUNT = "/run/fiberd"
# What `?dirty=N` wrote, kept so the pages stay the fiber's.
KEPT = []
# What `?spin=1` says, to the helper's log, as it starts waiting.
WAITING = "helper-fixture: waiting for the body"


def read(name: str):
    try:
        with open(f"{MOUNT}/{name}", "rb") as f:
            return f.read().decode()
    except FileNotFoundError:
        return None


def writable() -> bool:
    try:
        with open(f"{MOUNT}/written", "wb") as f:
            f.write(b"x")
        return True
    except OSError:
        return False


def spin(request: Request):
    print(WAITING, flush=True)
    try:
        request.body.bytes()
    except Exception:
        # The connection ended with the sandbox the fiber was parked from.
        pass
    while True:
        pass


def handle(request: Request, context: Context) -> Response:
    query = parse_qs(urlsplit(request.path).query)
    if query.get("spin"):
        spin(request)
    for n in query.get("dirty", []):
        # Every byte written, not zero pages a fresh allocation may map.
        KEPT.append(b"\x01" * int(n))
    incarnation = read("incarnation")
    return Response.json({
        "incarnation": json.loads(incarnation) if incarnation is not None else None,
        "payload": read("payload"),
        "writable": writable(),
    })


def handle_event(event: Event, context: ProtocolContext) -> Response:
    # The scaffolded service's events policy needs one; the tests send none.
    return Response.json({}, status=204)
