import sys
import hyperlight


def probe(request):
    print("provider stdout")
    print("provider stderr", file=sys.stderr)
    return {"echo": hyperlight.call("test.echo", request["value"])}


def denied(request):
    return hyperlight.call("not.authorized", request)


def runaway(request):
    while True:
        pass


def parked(request):
    import time
    time.sleep(60)
    return {}
