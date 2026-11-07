#!/usr/bin/env python3

import importlib.util
import pathlib
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
