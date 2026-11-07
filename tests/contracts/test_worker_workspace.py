#!/usr/bin/env python3
"""Identical application assertions against native dev or deployed HTTPS ingress."""

import argparse
import http.client
import json
import ssl
import time
import urllib.parse


class IngressHTTPSConnection(http.client.HTTPSConnection):
    def __init__(self, address, port, tls_hostname, **kwargs):
        super().__init__(address, port, **kwargs)
        self.tls_hostname = tls_hostname

    def connect(self):
        http.client.HTTPConnection.connect(self)
        self.sock = self._context.wrap_socket(
            self.sock, server_hostname=self.tls_hostname
        )


def open_connection(base, hostname, insecure):
    if base.scheme == "https":
        context = ssl._create_unverified_context() if insecure else ssl.create_default_context()
        tls_hostname = urllib.parse.urlsplit("//" + hostname).hostname
        assert tls_hostname
        return IngressHTTPSConnection(
            base.hostname,
            base.port,
            tls_hostname,
            timeout=15,
            context=context,
        )
    return http.client.HTTPConnection(base.hostname, base.port, timeout=15)


def sibling_hostname(hostname, service):
    selected = urllib.parse.urlsplit("//" + hostname)
    selected_hostname = selected.hostname
    assert selected_hostname
    _, separator, domain = selected_hostname.partition(".")
    assert separator and domain
    sibling = f"{service}.{domain}"
    if selected.port is not None:
        sibling += f":{selected.port}"
    return sibling


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--url", required=True)
    parser.add_argument("--host", help="Ingress hostname override for a loopback or cluster-IP connection")
    parser.add_argument("--insecure", action="store_true", help="Accept a test cluster's self-signed certificate")
    parser.add_argument("--direct-services", action="store_true", help="Also check processor/reused hostnames when explicitly exposed")
    args = parser.parse_args()
    base = urllib.parse.urlsplit(args.url)
    assert base.scheme in ("http", "https") and base.hostname
    hostname = args.host or ("worker-api.default.localhost" if base.scheme == "http" else base.netloc)

    def connect():
        return open_connection(base, hostname, args.insecure)

    def request(path, status=200, host=None, content_type=False):
        connection = connect()
        connection.request("GET", path, headers={"Host": host or hostname})
        response = connection.getresponse()
        body = response.read()
        connection.close()
        assert response.status == status, (path, response.status, body)
        if content_type is not False:
            assert response.getheader("content-type") == content_type, (
                path, response.getheader("content-type"), body
            )
        return body

    def value(path):
        return json.loads(request(path))

    roots = [value("/") for _ in range(2)]
    assert all(root["count"] == 1 and root["fiberId"] for root in roots), roots
    assert roots[0]["fiberId"] != roots[1]["fiberId"], "primary reused a fresh Fiber ID"
    invoked = [value("/invoke") for _ in range(2)]
    assert all(result["kind"] == "http" and result["count"] == 1 and result["fiberId"] for result in invoked), invoked
    assert invoked[0]["fiberId"] != invoked[1]["fiberId"], "processor reused a fresh Fiber ID"
    event = value("/emit")
    assert event.pop("fiberId"), event
    assert event == {
        "kind": "event", "id": "worker-demo",
        "data": {"message": "hello from HTTP ingress"}, "count": 1,
    }, event
    time.sleep(3)
    first_reuse = value("/reuse")
    second_reuse = value("/reuse")
    assert first_reuse["count"] == 1 and first_reuse["fiberId"]
    assert second_reuse["count"] == 2, "explicit reuse lost live state"
    assert second_reuse["fiberId"] == first_reuse["fiberId"], "reuse changed Fiber ID"
    time.sleep(3)
    reset_reuse = value("/reuse")
    assert reset_reuse["count"] == 1, "idle timeout did not release"
    assert reset_reuse["fiberId"] != first_reuse["fiberId"], "idle release retained Fiber ID"
    request("/processor/error", 500, content_type=None)
    request("/processor/disconnect", 502)
    start = time.monotonic()
    assert request("/processor/stream", content_type="text/plain") == b"start\nend\n"
    assert time.monotonic() - start >= 1.4, "stream ended prematurely"
    connection = connect()
    connection.request("GET", "/processor/stream", headers={"Host": hostname})
    response = connection.getresponse()
    assert response.status == 200 and response.readline() == b"start\n"
    # Close the response too: HTTPResponse owns the socket after getresponse().
    response.close()
    connection.close()
    time.sleep(0.3)
    after_cancel = value("/invoke")
    assert after_cancel["count"] == 1 and after_cancel["fiberId"], "request after cancellation did not reset"
    if base.scheme == "http":
        request("/", 404, host="undeclared.default.localhost")
    if args.direct_services:
        processor_hostname = sibling_hostname(hostname, "worker-processor")
        reused_hostname = sibling_hostname(hostname, "worker-reused")
        for _ in range(2):
            direct = json.loads(request("/count", host=processor_hostname))
            assert direct["kind"] == "http" and direct["count"] == 1 and direct["fiberId"], direct
        time.sleep(3)
        assert json.loads(request("/count", host=reused_hostname))["count"] == 1
        assert json.loads(request("/count", host=reused_hostname))["count"] == 2
    print("PASS: fresh state, bindings, emitted CloudEvent, reuse/idle, errors, streaming and cancellation")


if __name__ == "__main__":
    main()
