#!/usr/bin/env python3

import importlib.util
import json
import pathlib
import sys
import unittest
from unittest import mock
import urllib.parse


ROOT = pathlib.Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "worker_workspace_contract",
    ROOT / "tests" / "contracts" / "test_worker_workspace.py",
)
CONTRACT = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CONTRACT)


class WorkerWorkspaceConnectionTests(unittest.TestCase):
    def test_direct_requests_use_sibling_tls_identity(self):
        cases = (
            ("https://worker-api.default.example.com:8443", None, ":8443"),
            ("https://127.0.0.1:8443", "worker-api.default.example.com:9443", ":9443"),
            ("https://127.0.0.1:8443", "worker-api.default.example.com", ""),
        )
        for url, override, port in cases:
            with self.subTest(host=override):
                requests = self.run_contract(url, override)
                for service, count in (("worker-processor", 2), ("worker-reused", 2)):
                    with self.subTest(service=service):
                        hostname = f"{service}.default.example.com"
                        direct = [
                            request
                            for request in requests
                            if request["host"] == f"{hostname}{port}"
                        ]
                        self.assertEqual(len(direct), count)
                        for request in direct:
                            self.assertEqual(
                                request["address"], urllib.parse.urlsplit(url).hostname
                            )
                            self.assertEqual(request["port"], 8443)
                            self.assertEqual(request["tls_hostname"], hostname)

    def test_direct_http_requests_keep_native_destination(self):
        requests = self.run_contract("http://127.0.0.1:8080")
        for service in ("worker-processor", "worker-reused"):
            direct = [
                request
                for request in requests
                if request["host"] == f"{service}.default.localhost"
            ]
            self.assertEqual(len(direct), 2)
            for request in direct:
                self.assertEqual(request["address"], "127.0.0.1")
                self.assertEqual(request["port"], 8080)
                self.assertIsNone(request["tls_hostname"])

    def run_contract(self, url, host=None):
        replies = [
            {"count": 1, "fiberId": "root-1"},
            {"count": 1, "fiberId": "root-2"},
            {"kind": "http", "count": 1, "fiberId": "processor-1"},
            {"kind": "http", "count": 1, "fiberId": "processor-2"},
            {
                "kind": "event", "id": "worker-demo", "count": 1,
                "data": {"message": "hello from HTTP ingress"}, "fiberId": "event",
            },
            {"count": 1, "fiberId": "reused"},
            {"count": 2, "fiberId": "reused"},
            {"count": 1, "fiberId": "reset"},
            None, None, None, None,
            {"count": 1, "fiberId": "after-cancel"},
        ]
        statuses = [200] * len(replies)
        statuses[8:10] = [500, 502]
        if url.startswith("http:"):
            replies.append(None)
            statuses.append(404)
        replies.extend([
            {"kind": "http", "count": 1, "fiberId": "direct-processor-1"},
            {"kind": "http", "count": 1, "fiberId": "direct-processor-2"},
            {"count": 1},
            {"count": 2},
        ])
        statuses.extend([200] * 4)
        responses = []
        for reply, status in zip(replies, statuses):
            response = mock.Mock(status=status)
            response.read.return_value = json.dumps(reply).encode()
            response.getheader.return_value = None
            responses.append(response)
        responses[10].read.return_value = b"start\nend\n"
        responses[10].getheader.return_value = "text/plain"
        responses[11].readline.return_value = b"start\n"
        requests = []
        context = CONTRACT.ssl.create_default_context()

        def send(connection, method, path, headers):
            connection.connect()
            requests.append({
                "address": connection.host,
                "port": connection.port,
                "host": headers["Host"],
                "tls_hostname": (
                    wrap_socket.call_args.kwargs["server_hostname"]
                    if isinstance(connection, CONTRACT.IngressHTTPSConnection)
                    else None
                ),
            })

        argv = ["test_worker_workspace.py", "--url", url, "--direct-services"]
        if host is not None:
            argv.extend(["--host", host])
        with (
            mock.patch.object(sys, "argv", argv),
            mock.patch.object(CONTRACT.ssl, "create_default_context", return_value=context),
            mock.patch.object(context, "wrap_socket", return_value=mock.Mock()) as wrap_socket,
            mock.patch.object(CONTRACT.http.client.HTTPConnection, "connect"),
            mock.patch.object(
                CONTRACT.http.client.HTTPConnection, "request",
                autospec=True, side_effect=send,
            ),
            mock.patch.object(
                CONTRACT.http.client.HTTPConnection, "getresponse", side_effect=responses,
            ),
            mock.patch.object(CONTRACT.time, "sleep"),
            mock.patch.object(CONTRACT.time, "monotonic", side_effect=[0, 2]),
            mock.patch("builtins.print"),
        ):
            CONTRACT.main()
        return requests

    def test_https_host_override_is_used_for_tls_identity(self):
        base = urllib.parse.urlsplit("https://127.0.0.1:8443")
        context = mock.Mock()
        wrapped = object()
        context.wrap_socket.return_value = wrapped
        with (
            mock.patch.object(CONTRACT.ssl, "create_default_context", return_value=context),
            mock.patch.object(
                CONTRACT.http.client.HTTPConnection,
                "connect",
                lambda connection: setattr(connection, "sock", object()),
            ),
        ):
            connection = CONTRACT.open_connection(
                base, "worker-api.default.example.com", False
            )
            connection.connect()

        self.assertEqual(connection.host, "127.0.0.1")
        self.assertEqual(
            context.wrap_socket.call_args.kwargs["server_hostname"],
            "worker-api.default.example.com",
        )
        self.assertIs(connection.sock, wrapped)

    def test_sibling_hostname_preserves_deployed_domain_and_port(self):
        self.assertEqual(
            CONTRACT.sibling_hostname(
                "worker-api.default.example.com:8443", "worker-processor"
            ),
            "worker-processor.default.example.com:8443",
        )


if __name__ == "__main__":
    unittest.main()
