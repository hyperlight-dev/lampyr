"""Reusable wire harness: python3 tests/contracts/test_contract.py [--ly PATH]."""
import argparse
from contextlib import contextmanager
import http.client
import json
import os
from pathlib import Path
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
FIXTURES = Path(__file__).resolve().parent
LY = ROOT / "cli/target/debug/ly"
VM = False
RUNTIMES = {"nodejs": ("node", "js"), "python": ("python3", "py"),
            "go": ("go", "go"), "rust": ("cargo", "rs"), "dotnet": ("dotnet", "cs")}
NATIVE_CACHE = None


def contract_for(*runtimes, native_only=False):
    """Declare a contract's runtime and transport.

    Process-local state and adapter concurrency limits require native_only:
    snapshot dev routes concurrent requests to independent Fibers.
    """
    # A misspelled runtime would otherwise select the contract for no runtime at all.
    unknown = set(runtimes) - set(RUNTIMES)
    if unknown:
        raise ValueError(f"unknown contract runtimes: {sorted(unknown)}")

    def decorate(test):
        test.contract_runtimes = frozenset(runtimes or RUNTIMES)
        test.native_only = native_only
        return test
    return decorate


def applicable_contracts(tests):
    suite = unittest.TestSuite()
    for test in tests:
        if isinstance(test, unittest.TestSuite):
            suite.addTests(applicable_contracts(test))
        elif isinstance(test, Contract):
            method = getattr(test, test._testMethodName)
            runtimes = getattr(method, "contract_runtimes", frozenset(RUNTIMES))
            if runtimes.intersection(RUNTIMES) and not (VM and getattr(method, "native_only", False)):
                suite.addTest(test)
        else:
            suite.addTest(test)
    return suite


def require_contracts(tests):
    suite = applicable_contracts(tests)
    if not suite.countTestCases():
        raise ValueError("no contracts match the selected runtimes and transport")
    return suite


def load_tests(loader, tests, pattern):
    return require_contracts(loader.loadTestsFromTestCase(Contract))


class ContractLoader(unittest.TestLoader):
    def loadTestsFromNames(self, names, module=None):
        return require_contracts(super().loadTestsFromNames(names, module))


def handler_name(extension):
    return "Handler.cs" if extension == "cs" else f"handler.{extension}"


@contextmanager
def without_host_toolchains():
    with tempfile.TemporaryDirectory() as temporary:
        for binary in ("node", "python3", "go", "cargo", "rustc", "dotnet"):
            stub = Path(temporary) / binary
            stub.write_text(
                "#!/bin/sh\n"
                "echo 'host language toolchain must not be invoked' >&2\n"
                "exit 97\n")
            stub.chmod(0o755)
        yield {**os.environ, "PATH": temporary + os.pathsep + os.environ.get("PATH", "")}


def native_command(directory, binary, extension):
    if extension in ("js", "py"):
        return [binary, f".lampyr-adapter/server.{extension}"]
    output = directory.parent / "native"
    output.mkdir(exist_ok=True)
    if extension == "go":
        adapter = directory / ".lampyr-adapter"
        overlay = output / "overlay.json"
        overlay.write_text(json.dumps({"Replace": {
            str(adapter / source.name): str(source)
            for source in directory.glob("*.go")
        }}))
        shutil.copyfile(adapter / "go.mod", output / "go.mod")
        shutil.copyfile(adapter / "go.sum", output / "go.sum")
        compile_command = [binary, "build", "-mod=readonly", f"-modfile={output / 'go.mod'}",
                           f"-overlay={overlay}", "-o", str(output / "server"), "."]
        cwd = adapter
        command = [str(output / "server")]
    elif extension == "rs":
        target = NATIVE_CACHE / "rust"
        compile_command = [binary, "build", "--locked", "--manifest-path", ".lampyr-adapter/Cargo.toml",
                           "--target-dir", str(target)]
        cwd = directory
        command = [str(target / "debug/lampyr-service")]
    else:
        compile_command = [binary, "build", ".lampyr-adapter/LampyrService.csproj", "--nologo",
                           "--verbosity", "quiet", "--artifacts-path", str(output), "--output", str(output / "out")]
        cwd = directory
        command = [binary, str(output / "out/LampyrService.dll")]
    result = subprocess.run(compile_command, cwd=cwd, capture_output=True, text=True)
    if result.returncode:
        raise AssertionError(f"native compile failed:\n{result.stdout}\n{result.stderr}")
    return command


@contextmanager
def generated(runtime, fixture=False):
    with tempfile.TemporaryDirectory() as temporary:
        directory = Path(temporary) / "service"
        subprocess.run([str(LY), "scaffold", "service", "service", "--runtime", runtime],
                       cwd=temporary, check=True, stdout=subprocess.DEVNULL)
        binary, extension = RUNTIMES[runtime]
        if fixture:
            shutil.copyfile(FIXTURES / handler_name(extension), directory / handler_name(extension))
            manifest = directory / "lampyr.yaml"
            source = manifest.read_text()
            manifest.write_text(source[:source.index("events:")] + source[source.index("resources:"):])
        yield directory, binary, extension


@contextmanager
def serving(directory, binary, extension, output=None, on_stop=None):
    with socket.socket() as reserve:
        reserve.bind(("127.0.0.1", 0))
        port = reserve.getsockname()[1]
    with tempfile.TemporaryFile() as logs, without_host_toolchains() as container_env:
        command = ([str(LY), "dev", "--port", str(port), str(directory)]
                   if VM else native_command(directory, binary, extension))
        process = subprocess.Popen(command, cwd=directory,
                                   env={**(container_env if VM else os.environ),
                                        "PORT": str(port), "PYTHONDONTWRITEBYTECODE": "1"},
                                   stdout=logs, stderr=logs)
        try:
            deadline = time.monotonic() + (180 if VM else 10)
            while True:
                if process.poll() is not None or time.monotonic() >= deadline:
                    logs.seek(0)
                    raise AssertionError(logs.read().decode())
                try:
                    with socket.create_connection(("127.0.0.1", port), timeout=0.1):
                        break
                except OSError:
                    time.sleep(0.02)
            yield port
        finally:
            if process.poll() is None:
                process.terminate()
                if on_stop is not None:
                    on_stop()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
                raise AssertionError("adapter did not shut down")
            if output is not None:
                logs.seek(0)
                output.append(logs.read().decode())


def host_headers(headers=None):
    return {**({"Host": "service.default.localhost"} if VM else {}), **(headers or {})}


def call(port, path, method="GET", body=None, headers=None):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
    try:
        connection.request(method, path, body=body, headers=host_headers(headers))
        response = connection.getresponse()
        return response.status, {key.lower(): value for key, value in response.getheaders()}, response.read()
    finally:
        connection.close()


def oversized_status(port, path, size):
    """Return the status for a declared over-limit body without sending it.

    The contract is that adapters reject an over-limit Content-Length before
    reading the body, and they may close without draining it, so sending the
    body would race the 413 against EPIPE or a reset that discards the reply.
    An adapter that waits for the body yields None instead of a status.
    """
    with socket.create_connection(("127.0.0.1", port), timeout=5) as connection:
        connection.sendall(f"POST {path} HTTP/1.1\r\nHost: service.default.localhost\r\nContent-Length: {size}\r\n"
                           "Connection: close\r\n\r\n".encode())
        # Parse and consume the whole response, so a hang or a truncated 413
        # still fails the test.
        response = http.client.HTTPResponse(connection, method="POST")
        try:
            response.begin()
            response.read()
        except (socket.timeout, http.client.HTTPException):
            return None
        return response.status


def websocket_frame_size(buffer):
    """Size of the unmasked server frame at the start of buffer, once its header is complete."""
    length = buffer[1] & 0x7F
    if length == 126:
        return 4 + struct.unpack("!H", buffer[2:4])[0] if len(buffer) >= 4 else 4
    if length == 127:
        return 10 + struct.unpack("!Q", buffer[2:10])[0] if len(buffer) >= 10 else 10
    return 2 + length


def websocket_frame(opcode, data, fin=True):
    mask = b"mask"
    length = len(data)
    if length < 126:
        encoded_length = bytes([0x80 | length])
    elif length <= 65535:
        encoded_length = b"\xfe" + struct.pack("!H", length)
    else:
        encoded_length = b"\xff" + struct.pack("!Q", length)
    header = bytes([(0x80 if fin else 0) | opcode]) + encoded_length
    return header + mask + bytes(value ^ mask[index % 4] for index, value in enumerate(data))


EVENT_HEADERS = {"ce-specversion": "1.0", "ce-id": "contract-id",
                 "ce-type": "contract.test", "ce-source": "/contract",
                 "content-type": "application/json", "x-lampyr-delivery-attempt": "2"}


class Contract(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        temporary = tempfile.TemporaryDirectory()
        cls.addClassCleanup(temporary.cleanup)
        dependencies = Path(temporary.name)
        node = dependencies / "node"
        node.mkdir()
        for name in ("package.json", "package-lock.json"):
            shutil.copyfile(ROOT / "templates/services/nodejs/adapter" / name, node / name)
        subprocess.run(["npm", "ci", "--omit=dev", "--omit=optional", "--ignore-scripts",
                        "--no-audit", "--no-fund"], cwd=node, check=True, stdout=subprocess.DEVNULL)
        python = dependencies / "python"
        subprocess.run([sys.executable, "-m", "pip", "install", "--quiet", "--no-compile",
                        "--require-hashes", "--only-binary=:all:", "--target", str(python),
                        "-r", str(ROOT / "templates/services/python/adapter/requirements.txt")], check=True)
        environment = patch.dict(os.environ, {"NODE_PATH": str(node / "node_modules"), "PYTHONPATH": str(python)})
        environment.start()
        cls.addClassCleanup(environment.stop)

    @contract_for(native_only=True)
    def test_listener_respects_dev_bind_host(self):
        for runtime in RUNTIMES:
            with self.subTest(runtime=runtime), generated(runtime) as (directory, binary, extension):
                with patch.dict(os.environ, {"LAMPYR_LISTEN_HOST": "127.0.0.1"}):
                    with serving(directory, binary, extension) as port:
                        self.assertEqual(call(port, "/hello")[0], 200)
                        with self.assertRaises(OSError):
                            with socket.create_connection(("127.0.0.2", port), timeout=1):
                                self.fail("adapter exposed a wildcard listener")

    def test_shared_wire_cases(self):
        for runtime in RUNTIMES:
            with self.subTest(runtime=runtime), generated(runtime, fixture=True) as (directory, binary, extension):
                with serving(directory, binary, extension) as port:
                    for case in json.loads((FIXTURES / "cases.json").read_text()):
                        with self.subTest(case=case["path"]):
                            status, headers, body = call(port, case["path"])
                            self.assertEqual(status, case["status"])
                            if "body" in case:
                                self.assertEqual(json.loads(body), case["body"])
                            if "text" in case:
                                self.assertEqual(body.decode(), case["text"])
                            if "header" in case:
                                self.assertEqual(headers.get(case["header"][0]), case["header"][1])
                            self.assertNotIn(b"private details", body)
                    status, _, body = call(port, "/echo?q=yes", "POST", b"payload", {"X-Test": "mixed"})
                    self.assertEqual(status, 200)
                    self.assertEqual(json.loads(body), {
                        "method": "POST", "path": "/echo?q=yes", "body": "payload", "header": "mixed"})
                    self.assertEqual(call(port, "/value", "HEAD")[2], b"")
                    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
                    connection.request("GET", "/text", headers=host_headers())
                    response = connection.getresponse()
                    self.assertEqual(response.headers.get_all("content-type"), ["text/plain"])
                    self.assertEqual(response.read(), b"plain text")
                    connection.close()
                    if runtime == "python":
                        _, headers, _ = call(port, "/empty")
                        self.assertNotIn("content-length", headers)
                        _, headers, _ = call(port, "/not-modified")
                        self.assertNotIn("content-length", headers)
                        _, headers, _ = call(port, "/value", "HEAD")
                        self.assertNotIn("content-length", headers)
                    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
                    connection.request("GET", "/typed-error", headers=host_headers())
                    response = connection.getresponse()
                    self.assertEqual(response.status, 422)
                    self.assertIn(response.reason, ("Unprocessable Entity", "Unprocessable Content"))
                    self.assertNotIn("x-injected", {key.lower() for key in response.headers})
                    self.assertEqual(response.getheader("x-content-type-options"), "nosniff")
                    if runtime == "rust":
                        self.assertEqual(response.getheader("content-type"), "text/plain; charset=utf-8")
                    error_body = response.read()
                    if runtime == "python":
                        self.assertIn(b"&lt;bad&gt;", error_body)
                    elif runtime == "dotnet":
                        self.assertEqual(json.loads(error_body)["error"], "<bad>\r\nX-Injected: yes")
                    else:
                        self.assertIn(b"<bad>", error_body)
                    connection.close()
                    self.assertEqual(call(port, "/direct", "POST", b"{}", EVENT_HEADERS)[0], 404)
                    self.assertEqual(call(port, "/direct", "POST", b"{}",
                                          {"Content-Type": "Application/CloudEvents+JSON"})[0], 404)
                    self.assertEqual(call(port, "/ws", headers={
                        "Upgrade": "websocket", "Connection": "Upgrade",
                        "Sec-WebSocket-Version": "13",
                        "Sec-WebSocket-Key": "dGhlIHNhbXBsZSBub25jZQ==",
                    })[0], 404)
                    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
                    connection.request("GET", "/broken-stream", headers=host_headers())
                    try:
                        response = connection.getresponse()
                        body = response.read()
                    except (http.client.IncompleteRead, http.client.RemoteDisconnected, ConnectionResetError):
                        pass
                    else:
                        self.assertTrue(VM and response.status == 502,
                                        ("broken stream completed without a transport error", response.status, body))
                    connection.close()
                    connection = socket.create_connection(("127.0.0.1", port), timeout=5)
                    connection.sendall(b"GET /cancel HTTP/1.1\r\nHost: service.default.localhost\r\n\r\n")
                    connection.recv(4096)
                    connection.close()
                    if not VM:
                        deadline = time.monotonic() + 3
                        while not (directory / "cancelled").exists() and time.monotonic() < deadline:
                            time.sleep(0.02)
                        self.assertTrue((directory / "cancelled").exists(), "stream iterator was not closed")
                    self.assertEqual(call(port, "/value")[0], 201)

    @contract_for("dotnet")
    def test_dotnet_extension_method_is_preserved(self):
        with generated("dotnet", fixture=True) as (directory, binary, extension):
            with serving(directory, binary, extension) as port:
                for method in ("OST", "head", "Head"):
                    status, _, body = call(port, "/echo", method)
                    self.assertEqual(status, 200)
                    self.assertEqual(json.loads(body)["method"], method)

    @contract_for("dotnet")
    def test_dotnet_typed_json_roundtrip(self):
        with generated("dotnet", fixture=True) as (directory, binary, extension):
            with serving(directory, binary, extension) as port:
                status, _, serialized = call(port, "/typed-json")
                self.assertEqual(status, 200)
                expected = {"message": "hello", "count": 7}
                self.assertEqual(json.loads(serialized), expected)
                for payload in (serialized, b'{"Message":"hello","Count":7}', b'{"MESSAGE":"hello","COUNT":7}'):
                    with self.subTest(payload=payload):
                        status, _, body = call(port, "/typed-json", "POST", payload,
                                               {"content-type": "application/json"})
                        self.assertEqual(status, 200)
                        self.assertEqual(json.loads(body), expected)

    @contract_for("go", native_only=True)
    def test_go_blocking_stream_cleanup(self):
        with generated("go", fixture=True) as (directory, binary, _):
            adapter = directory / ".lampyr-adapter"
            shutil.copyfile(directory / "handler.go", adapter / "handler.go")
            shutil.copyfile(FIXTURES / "stream_test.go", adapter / "stream_test.go")
            result = subprocess.run([binary, "test", "-race", "-count=1", "-timeout=30s", "."],
                                    cwd=adapter, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    @contract_for("go", "rust", "dotnet")
    def test_compiled_websocket_hook_return_sends_close_frame(self):
        for runtime in ("go", "rust", "dotnet"):
            if runtime not in RUNTIMES:
                continue
            with self.subTest(runtime=runtime), generated(runtime, fixture=True) as (directory, binary, extension):
                shutil.copyfile(FIXTURES / f"websocket-handler.{extension}", directory / handler_name(extension))
                with serving(directory, binary, extension) as port:
                    cases = [("/complete", 1000, ""), ("/fail", 1011, "request failed"),
                             ("/close", 4000, "finished")]
                    if runtime in ("go", "rust"):
                        cases.append(("/panic", 1011, "request failed"))
                    if runtime == "go":
                        cases.append(("/close-panic", 4000, "finished"))
                    for path, code, reason in cases:
                        with self.subTest(path=path), socket.create_connection(
                                ("127.0.0.1", port), timeout=3) as connection:
                            frame = self.upgrade_websocket(connection, path)
                            self.assert_websocket_close(connection, frame, code, reason)

    @contract_for("rust", native_only=True)
    def test_rust_websocket_peer_close_interrupts_blocked_sender(self):
        with generated("rust", fixture=True) as (directory, binary, extension):
            shutil.copyfile(FIXTURES / "websocket-handler.rs", directory / "handler.rs")
            with serving(directory, binary, extension) as port:
                with socket.create_connection(("127.0.0.1", port), timeout=3) as connection:
                    connection.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4096)
                    self.upgrade_websocket(connection, "/send")
                    deadline = time.monotonic() + 3
                    while True:
                        state = json.loads(call(port, "/state")[2])
                        self.assertLess(time.monotonic(), deadline, "WebSocket sender did not block")
                        if state["sent"] > 0:
                            self.assertTrue(state["active"], "WebSocket sender stopped before peer close")
                            time.sleep(0.1)
                            later = json.loads(call(port, "/state")[2])
                            self.assertTrue(later["active"], "WebSocket sender stopped before peer close")
                            if later["sent"] == state["sent"]:
                                break
                        else:
                            time.sleep(0.01)
                    mask = b"mask"
                    message = b"\x81\x81" + mask + bytes([ord("a") ^ mask[0]])
                    close = b"\x88\x82" + mask + bytes(
                        value ^ mask[index % 4] for index, value in enumerate(b"\x03\xe8"))
                    connection.sendall(message + close)
                    deadline = time.monotonic() + 3
                    while not json.loads(call(port, "/state")[2])["disconnected"]:
                        self.assertLess(time.monotonic(), deadline, "blocked writer prevented hook cancellation")
                        time.sleep(0.01)
                    self.assertFalse(json.loads(call(port, "/state")[2])["active"])

    @contract_for("rust", native_only=True)
    def test_rust_websocket_cancelled_send_reports_error(self):
        with generated("rust", fixture=True) as (directory, binary, extension):
            shutil.copyfile(FIXTURES / "websocket-handler.rs", directory / "handler.rs")
            with serving(directory, binary, extension) as port:
                with socket.create_connection(("127.0.0.1", port), timeout=3) as connection:
                    frame = self.upgrade_websocket(connection, "/cancel-send")
                    self.assert_websocket_close(connection, frame, 1000, "")
                self.assertTrue(json.loads(call(port, "/state")[2])["cancelledSendFailed"],
                                "cancelled send did not return ConnectionClosed")

    @contract_for("rust", native_only=True)
    def test_rust_websocket_shutdown_interrupts_blocked_sender(self):
        with generated("rust", fixture=True) as (directory, binary, extension):
            shutil.copyfile(FIXTURES / "websocket-handler.rs", directory / "handler.rs")
            connection = None
            try:
                with serving(directory, binary, extension) as port:
                    connection = socket.create_connection(("127.0.0.1", port), timeout=3)
                    connection.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4096)
                    self.upgrade_websocket(connection, "/send")
                    deadline = time.monotonic() + 3
                    while json.loads(call(port, "/state")[2])["sent"] == 0:
                        self.assertLess(time.monotonic(), deadline, "WebSocket sender did not start")
                        time.sleep(0.01)
            finally:
                if connection is not None:
                    connection.close()

    @contract_for("go", "rust", "dotnet")
    def test_duplicate_cloudevent_content_type_is_rejected_in_any_order(self):
        for runtime in ("go", "rust", "dotnet"):
            if runtime not in RUNTIMES:
                continue
            # The default templates register an event hook, so a misclassified
            # request would reach Handle and succeed instead of failing with 400.
            with self.subTest(runtime=runtime), generated(runtime) as (directory, binary, extension):
                with serving(directory, binary, extension) as port:
                    for first, second in (("application/cloudevents+json", "text/plain"),
                                          ("text/plain", "application/cloudevents+json")):
                        body = b'{"specversion":"1.0","id":"x","type":"t","source":"/s","data":{}}'
                        with socket.create_connection(("127.0.0.1", port), timeout=5) as connection:
                            connection.sendall(
                                f"POST /events HTTP/1.1\r\nHost: service.default.localhost\r\nContent-Type: {first}\r\n"
                                f"Content-Type: {second}\r\nContent-Length: {len(body)}\r\n"
                                "Connection: close\r\n\r\n".encode() + body)
                            response = b""
                            while chunk := connection.recv(4096):
                                response += chunk
                        status = response.split(b"\r\n", 1)[0]
                        self.assertIn(b" 400 ", status, (first, second, response[:200]))

    @contract_for("go", "rust", "dotnet")
    def test_absolute_form_targets_use_origin_path(self):
        for runtime in ("go", "rust", "dotnet"):
            if runtime not in RUNTIMES:
                continue
            with self.subTest(runtime=runtime), generated(runtime) as (directory, binary, extension):
                with serving(directory, binary, extension) as port:
                    # http.client sends an absolute URL verbatim as the request target.
                    host = "service.default.localhost" if VM else "h"
                    status, _, body = call(port, f"http://{host}/admin?q=1", headers={"Host": host})
                    self.assertEqual(status, 200, body)
                    self.assertEqual(json.loads(body)["path"], "/admin?q=1")

    @contract_for("rust")
    def test_rust_plain_requests_skip_event_header_validation(self):
        with generated("rust", fixture=True) as (directory, binary, extension):
            with serving(directory, binary, extension) as port:
                for name, headers in (("duplicate", b"Content-Type: text/plain\r\nContent-Type: text/plain\r\n"),
                                      ("non-ascii", "Content-Type: text/plain; x=\u00e9\r\n".encode("latin-1"))):
                    with self.subTest(case=name), socket.create_connection(("127.0.0.1", port), timeout=3) as connection:
                        connection.sendall(b"POST /echo HTTP/1.1\r\nHost: service.default.localhost\r\nContent-Length: 2\r\n"
                                           + headers + b"Connection: close\r\n\r\nhi")
                        status_line = connection.recv(4096).split(b"\r\n", 1)[0]
                        self.assertEqual(status_line.split()[1], b"200", status_line)

    @contract_for("rust", native_only=True)
    def test_rust_websocket_data_frame_during_stalled_send_keeps_session(self):
        with generated("rust", fixture=True) as (directory, binary, extension):
            shutil.copyfile(FIXTURES / "websocket-handler.rs", directory / "handler.rs")
            with serving(directory, binary, extension) as port:
                with socket.create_connection(("127.0.0.1", port), timeout=3) as connection:
                    connection.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4096)
                    self.upgrade_websocket(connection, "/send")
                    deadline = time.monotonic() + 3
                    while json.loads(call(port, "/state")[2])["sent"] == 0:
                        self.assertLess(time.monotonic(), deadline, "WebSocket sender did not start")
                        time.sleep(0.01)
                    time.sleep(1.5)  # let send() stall on the unread socket
                    connection.sendall(websocket_frame(0x1, b"hi"))
                    connection.sendall(websocket_frame(0x9, b"ping"))
                    time.sleep(2)  # longer than the reader's 1 s flush timeout
                    state = json.loads(call(port, "/state")[2])
                    self.assertTrue(state["active"], state)
                    self.assertFalse(state["disconnected"], state)

    @contract_for("rust", native_only=True)
    def test_rust_websocket_keepalive_pings_do_not_overload_idle_hooks(self):
        with generated("rust", fixture=True) as (directory, binary, extension):
            shutil.copyfile(FIXTURES / "websocket-handler.rs", directory / "handler.rs")
            with serving(directory, binary, extension) as port:
                with socket.create_connection(("127.0.0.1", port), timeout=3) as connection:
                    self.upgrade_websocket(connection, "/ws")
                    connection.sendall(websocket_frame(0x9, b"keepalive") * 20)
                    time.sleep(1)
                    state = json.loads(call(port, "/state")[2])
                    self.assertTrue(state["active"], state)
                    self.assertFalse(state["disconnected"], state)

    @contract_for("rust")
    def test_rust_websocket_idle_close_frames(self):
        with generated("rust", fixture=True) as (directory, binary, extension):
            shutil.copyfile(FIXTURES / "websocket-handler.rs", directory / "handler.rs")
            with serving(directory, binary, extension) as port:
                mask = b"mask"
                peer_close = struct.pack("!H", 4001) + b"peer finished"
                cases = [
                    ("peer close", bytes([0x88, 0x80 | len(peer_close)]) + mask + bytes(
                        value ^ mask[index % 4] for index, value in enumerate(peer_close)),
                     4001, "peer finished"),
                    ("overload", (b"\x81\x81" + mask + bytes([ord("a") ^ mask[0]])) * 17,
                     1013, "overloaded"),
                    ("invalid text", b"\x81\x81" + mask + bytes([0xff ^ mask[0]]), 1007, ""),
                    ("oversized", b"\x82\xff" + struct.pack("!Q", 1024 * 1024 + 1) + mask, 1009, ""),
                ]
                for name, incoming, code, reason in cases:
                    with self.subTest(case=name), socket.create_connection(
                            ("127.0.0.1", port), timeout=3) as connection:
                        frame = self.upgrade_websocket(connection, "/ws")
                        connection.sendall(incoming)
                        self.assert_websocket_close(connection, frame, code, reason)

    @contract_for("rust", native_only=True)
    def test_rust_websocket_shutdown_sends_close_frame(self):
        with generated("rust", fixture=True) as (directory, binary, extension):
            shutil.copyfile(FIXTURES / "websocket-handler.rs", directory / "handler.rs")
            connection = None
            try:
                with serving(directory, binary, extension) as port:
                    connection = socket.create_connection(("127.0.0.1", port), timeout=3)
                    frame = self.upgrade_websocket(connection, "/ws")
                    deadline = time.monotonic() + 3
                    while not json.loads(call(port, "/state")[2])["active"]:
                        self.assertLess(time.monotonic(), deadline, "WebSocket hook did not start")
                        time.sleep(0.01)
                self.assert_websocket_close(connection, frame, 1001, "server shutdown")
            finally:
                if connection is not None:
                    connection.close()

    @contract_for("dotnet", native_only=True)
    def test_dotnet_websocket_shutdown_interrupts_idle_sessions(self):
        # /send stays blocked on an unread socket (well within its 30 s send timeout), so only shutdown can release it.
        for path in ("/idle", "/send"):
            with self.subTest(path=path), generated("dotnet", fixture=True) as (directory, binary, extension):
                shutil.copyfile(FIXTURES / "websocket-handler.cs", directory / "Handler.cs")
                connection = None
                try:
                    with serving(directory, binary, extension) as port:
                        connection = socket.create_connection(("127.0.0.1", port), timeout=3)
                        connection.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4096)
                        frame = self.upgrade_websocket(connection, path)
                        deadline = time.monotonic() + 3
                        while True:
                            state = json.loads(call(port, "/state")[2])
                            if state["active"] == 1 and (path != "/send" or state["sent"] > 0):
                                break
                            self.assertLess(time.monotonic(), deadline, "WebSocket hook did not start")
                            time.sleep(0.01)
                    if path == "/idle":
                        self.assert_websocket_close(connection, frame, 1001, "server shutdown")
                    self.assertTrue((directory / "websocket-finished").exists(),
                                    "shutdown did not release the WebSocket hook")
                finally:
                    if connection is not None:
                        connection.close()

    @contract_for("dotnet", native_only=True)
    def test_dotnet_websocket_shutdown_is_clean(self):
        # /tick sends without watching its token; /idle with a replying peer must not wait out the close timeout.
        for path, reply, read_after_stop in (
                ("/tick", True, False), ("/idle", True, False), ("/send-context", True, True)):
            with self.subTest(path=path), generated("dotnet", fixture=True) as (directory, binary, extension):
                shutil.copyfile(FIXTURES / "websocket-handler.cs", directory / "Handler.cs")
                logs = []
                connection = None
                closed = threading.Event()
                shutdown = threading.Event()
                frames = []

                def read_until_close(buffer):
                    try:
                        if read_after_stop:
                            shutdown.wait()
                        while True:
                            while len(buffer) < 2 or len(buffer) < websocket_frame_size(buffer):
                                chunk = connection.recv(65536)
                                if not chunk:
                                    return
                                buffer += chunk
                            size = websocket_frame_size(buffer)
                            frame, buffer = buffer[:size], buffer[size:]
                            frames.append(frame)
                            if frame[0] & 0x0F == 8:
                                if reply:
                                    connection.sendall(websocket_frame(8, frame[2:]))
                                return
                    except OSError:
                        pass
                    finally:
                        closed.set()

                try:
                    with serving(directory, binary, extension, logs,
                                 shutdown.set if read_after_stop else None) as port:
                        connection = socket.create_connection(("127.0.0.1", port), timeout=5)
                        if path == "/send-context":
                            connection.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 65536)
                        initial = self.upgrade_websocket(connection, path)
                        deadline = time.monotonic() + 3
                        while True:
                            state = json.loads(call(port, "/state")[2])
                            if state["active"] == 1:
                                break
                            self.assertLess(time.monotonic(), deadline, "WebSocket hook did not start")
                            time.sleep(0.01)
                        if path == "/send-context":
                            sent = -1
                            stalled_since = time.monotonic()
                            while time.monotonic() - stalled_since < 3:
                                current = json.loads(call(port, "/state")[2])["sent"]
                                if current != sent:
                                    sent = current
                                    stalled_since = time.monotonic()
                                elif sent > 0 and time.monotonic() - stalled_since >= 0.1:
                                    break
                                time.sleep(0.01)
                            self.assertGreater(sent, 0, "WebSocket sender did not start")
                            self.assertGreaterEqual(
                                time.monotonic() - stalled_since, 0.1, "WebSocket sender did not stall")
                        reader = threading.Thread(target=read_until_close, args=(initial,), daemon=True)
                        reader.start()
                        stopping = time.monotonic()
                    stopped = time.monotonic() - stopping
                    self.assertTrue(closed.wait(3), "WebSocket was not closed")
                    payload = struct.pack("!H", 1001) + b"server shutdown"
                    self.assertTrue(frames, f"no close frame; adapter logs: {logs}")
                    self.assertEqual(frames[-1], bytes([0x88, len(payload)]) + payload)
                    # A peer that replies ends the close at once; one that doesn't is cut off after 1 s.
                    self.assertLess(stopped, 0.9 if reply else 1.9, "shutdown did not end the hook promptly")
                    self.assertNotIn("request failed", logs[0])
                    self.assertNotIn("Exception", logs[0])
                    self.assertTrue((directory / "websocket-finished").exists(),
                                    "shutdown did not release the WebSocket hook")
                finally:
                    if connection is not None:
                        connection.close()

    @contract_for("dotnet", native_only=True)
    def test_dotnet_websocket_overload_releases_blocked_sender(self):
        with generated("dotnet", fixture=True) as (directory, binary, extension):
            shutil.copyfile(FIXTURES / "websocket-handler.cs", directory / "Handler.cs")
            with serving(directory, binary, extension) as port:
                with socket.create_connection(("127.0.0.1", port), timeout=3) as connection:
                    connection.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4096)
                    self.upgrade_websocket(connection, "/send")
                    deadline = time.monotonic() + 3
                    while json.loads(call(port, "/state")[2])["sent"] == 0:
                        self.assertLess(time.monotonic(), deadline, "WebSocket sender did not start")
                        time.sleep(0.01)
                    mask = b"mask"
                    frame = b"\x81\x81" + mask + bytes([ord("a") ^ mask[0]])
                    connection.sendall(frame * 17)
                    deadline = time.monotonic() + 3
                    while json.loads(call(port, "/state")[2])["active"] != 0:
                        self.assertLess(time.monotonic(), deadline, "overload left the WebSocket sender blocked")
                        time.sleep(0.01)
                    self.assertEqual(call(port, "/hello")[2], b"available")

    @contract_for("go", native_only=True)
    def test_go_websocket_transport_lifecycle(self):
        with generated("go", fixture=True) as (directory, binary, extension):
            shutil.copyfile(FIXTURES / "websocket-handler.go", directory / "handler.go")
            with serving(directory, binary, extension) as port:
                for path in ("/cancel", "/ws"):
                    with socket.create_connection(("127.0.0.1", port), timeout=5) as connection:
                        connection.sendall(
                            f"GET {path} HTTP/1.1\r\nHost: service.default.localhost\r\nUpgrade: websocket\r\n"
                            "Connection: Upgrade\r\nSec-WebSocket-Version: 13\r\n"
                            "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n".encode())
                        response = b""
                        while b"\r\n\r\n" not in response:
                            chunk = connection.recv(4096)
                            self.assertTrue(chunk, "connection closed before upgrade")
                            response += chunk
                        self.assertIn(b"101 Switching Protocols", response)
                        if path == "/cancel":
                            continue
                        def exact(length):
                            data = b""
                            while len(data) < length:
                                chunk = connection.recv(length - len(data))
                                self.assertTrue(chunk, "incomplete WebSocket frame")
                                data += chunk
                            return data
                        mask = b"mask"
                        connection.sendall(b"\x82\x81" + mask + bytes([0xff ^ mask[0]]))
                        self.assertEqual(exact(3), b"\x82\x01\xff")
                        connection.sendall(b"\x01\x81" + mask + bytes([ord("a") ^ mask[0]])
                                           + b"\x80\x81" + mask + bytes([0xff ^ mask[0]]))
                        opcode, length = exact(2)
                        self.assertEqual(opcode, 0x88)
                        self.assertEqual(exact(length)[:2], struct.pack("!H", 1007))
                deadline = time.monotonic() + 3
                while not json.loads(call(port, "/state")[2])["disconnected"]:
                    self.assertLess(time.monotonic(), deadline, "idle hook did not observe disconnect")
                    time.sleep(0.01)

    @contract_for("rust", native_only=True)
    def test_rust_websocket_disconnect_cancels_idle_hook_after_queued_messages(self):
        with generated("rust", fixture=True) as (directory, binary, extension):
            shutil.copyfile(FIXTURES / "websocket-handler.rs", directory / "handler.rs")
            with serving(directory, binary, extension) as port:
                with socket.create_connection(("127.0.0.1", port), timeout=5) as connection:
                    connection.sendall(
                        "GET /ws HTTP/1.1\r\nHost: service.default.localhost\r\nUpgrade: websocket\r\n"
                        "Connection: Upgrade\r\nSec-WebSocket-Version: 13\r\n"
                        "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n".encode())
                    response = b""
                    while b"\r\n\r\n" not in response:
                        chunk = connection.recv(4096)
                        self.assertTrue(chunk, "connection closed before upgrade")
                        response += chunk
                    self.assertIn(b"101 Switching Protocols", response)
                    mask = b"mask"
                    frame = b"\x81\x81" + mask + bytes([ord("a") ^ mask[0]])
                    connection.sendall(frame * 4)
                deadline = time.monotonic() + 3
                while not json.loads(call(port, "/state")[2])["disconnected"]:
                    self.assertLess(time.monotonic(), deadline, "idle hook did not observe disconnect")
                    time.sleep(0.01)

    @contract_for("dotnet")
    def test_dotnet_error_statuses_do_not_acknowledge_events(self):
        with generated("dotnet") as (directory, binary, extension):
            shutil.copyfile(FIXTURES / "error-handler.cs", directory / "Handler.cs")
            with serving(directory, binary, extension) as port:
                for headers in ({}, EVENT_HEADERS):
                    for requested in (-1, 200, 399, 400, 422, 599, 600, 999):
                        with self.subTest(event=bool(headers), status=requested):
                            status, _, body = call(port, f"/{requested}", "POST", b"{}", headers)
                            valid = 400 <= requested <= 599
                            self.assertEqual(status, requested if valid else 500)
                            self.assertEqual(json.loads(body)["error"],
                                             "typed error details" if valid else "request failed")

    @contract_for("dotnet")
    def test_dotnet_websocket_close_is_bounded(self):
        with generated("dotnet", fixture=True) as (directory, binary, extension):
            shutil.copyfile(FIXTURES / "websocket-handler.cs", directory / "Handler.cs")
            with serving(directory, binary, extension) as port:
                for path, code, reason in (("/complete", 1000, ""), ("/fail", 1011, "request failed"),
                                           ("/idle", 1000, "")):
                    with self.subTest(path=path), socket.create_connection(
                            ("127.0.0.1", port), timeout=3) as connection:
                        closing = self.upgrade_websocket(connection, path)
                        if path == "/idle":
                            connection.sendall(websocket_frame(8, struct.pack("!H", 1000)))
                        self.assert_websocket_close(connection, closing, code, reason)
                        self.assertEqual(call(port, "/hello")[2], b"available")

    @contract_for("dotnet")
    def test_dotnet_websocket_message_size_boundary(self):
        with generated("dotnet", fixture=True) as (directory, binary, extension):
            shutil.copyfile(FIXTURES / "websocket-handler.cs", directory / "Handler.cs")
            with serving(directory, binary, extension) as port:
                limit = 1024 * 1024
                for opcode in (1, 2):
                    for fragmented in (False, True):
                        for size in (limit, limit + 1):
                            with self.subTest(opcode=opcode, fragmented=fragmented, size=size), socket.create_connection(
                                    ("127.0.0.1", port), timeout=3) as connection:
                                closing = self.upgrade_websocket(connection, "/oversized")
                                payload = b"x" * size
                                chunk_size = 32768 if fragmented else size
                                frames = []
                                for offset in range(0, size, chunk_size):
                                    frames.append(websocket_frame(
                                        opcode if offset == 0 else 0,
                                        payload[offset:offset + chunk_size],
                                        fin=offset + chunk_size >= size))
                                connection.sendall(b"".join(frames))
                                code, reason = (1009, "message too large") if size > limit else (1000, "")
                                self.assert_websocket_close(connection, closing, code, reason)
                                self.assertEqual(call(port, "/hello")[2], b"available")

    def test_samples_and_events_without_policy(self):
        for runtime in RUNTIMES:
            with self.subTest(runtime=runtime), generated(runtime) as (directory, binary, extension):
                manifest = directory / "lampyr.yaml"
                source = manifest.read_text()
                start = source.index("events:")
                end = source.index("resources:")
                manifest.write_text(source[:start] + source[end:])
                with serving(directory, binary, extension) as port:
                    self.assertEqual(call(port, "/hello")[0], 200)
                    for path, status in [("/events", 200), ("/events/fail", 500), ("/events/knative", 204)]:
                        result, _, body = call(port, path, "POST", b'{"ok":true}', EVENT_HEADERS)
                        self.assertEqual(result, status)
                        if status != 204:
                            event = json.loads(body)["event"]
                            self.assertEqual(event["data"], {"ok": True})
                            self.assertEqual(event["attempt"], 2)
                    envelope = {"specversion": "1.0", "id": "structured", "type": "test",
                                "source": "/contract", "data": {"structured": True}}
                    for content_type in (None, "text/plain", "application/json"):
                        headers = {key: value for key, value in EVENT_HEADERS.items()
                                   if key != "content-type"}
                        if content_type is not None:
                            headers["content-type"] = content_type
                        for payload, expected in ((b'{"ok":true}', {"ok": True}),
                                                  (b"plain text", "plain text")):
                            status, _, body = call(port, "/events", "POST", payload, headers)
                            self.assertEqual(status, 200)
                            self.assertEqual(json.loads(body)["event"]["data"], expected)
                    for media_type in ("application/cloudevents+json",
                                       "Application/CloudEvents+JSON ; charset=utf-8"):
                        status, _, body = call(port, "/arbitrary", "POST", json.dumps(envelope),
                                               {"content-type": media_type})
                        self.assertEqual(status, 200)
                        self.assertEqual(json.loads(body)["event"], {
                            "id": "structured", "type": "test", "source": "/contract",
                            "subject": "", "time": "", "attempt": 1, "data": {"structured": True},
                        })
                    self.assertEqual(call(port, "/arbitrary", "POST", "{}",
                                          {"Content-Type": "Application/CloudEvents+JSON"})[0], 400)
                    for data_base64, include_data in [("cGF5bG9hZA==", False),
                                                      ("cGF5bG9hZA==", True), (None, False)]:
                        unsupported = {**envelope, "data_base64": data_base64}
                        if not include_data:
                            del unsupported["data"]
                        self.assertEqual(call(port, "/arbitrary", "POST", json.dumps(unsupported),
                                              {"content-type": "application/cloudevents+json"})[0], 400)
                    self.assertEqual(call(port, "/events", "POST", b'"\xff"', EVENT_HEADERS)[0], 400)
                    structured = (b'{"specversion":"1.0","id":"i","type":"t","source":"/s",'
                                  b'"data":"\xff"}')
                    self.assertEqual(call(port, "/events", "POST", structured,
                                          {"content-type": "application/cloudevents+json"})[0], 400)
                    self.assertEqual(call(port, "/events", "POST", "{}", {"ce-specversion": "1.0"})[0], 400)
                    bad = {**EVENT_HEADERS, "x-lampyr-delivery-attempt": "invalid"}
                    self.assertEqual(call(port, "/events", "POST", "{}", bad)[0], 400)
                    for invalid in ("null", "[]", '{"specversion":"1.0"}', "{"):
                        self.assertEqual(call(port, "/events", "POST", invalid,
                                              {"content-type": "application/cloudevents+json"})[0], 400)
                    for constant in ("NaN", "Infinity", "-Infinity"):
                        invalid = json.dumps(envelope).replace('{"structured": true}', constant)
                        self.assertEqual(call(port, "/events", "POST", invalid,
                                              {"content-type": "application/cloudevents+json"})[0], 400)
                    for encoding in ("utf-16", "utf-32"):
                        self.assertEqual(call(port, "/events", "POST", json.dumps(envelope).encode(encoding),
                                              {"content-type": "application/cloudevents+json"})[0], 400)
                    if runtime != "python":
                        self.assertEqual(call(port, "/stream")[2], b'{"chunk":1}\n{"chunk":2}\n{"chunk":3}\n')
                    event_host = "service.default.localhost" if VM else "127.0.0.1"
                    result = subprocess.run([str(LY), "event", "publish", "--url", f"http://{event_host}:{port}/events",
                                             "--data", '{"ok":true}', str(directory)],
                                            capture_output=True, text=True)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertFalse((directory / ".lampyr/dead-letter.jsonl").exists())
                    result = subprocess.run([str(LY), "event", "publish", "--url",
                                             f"http://{event_host}:{port}/events/fail", str(directory)],
                                            capture_output=True, text=True)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn("direct event", result.stderr)
                    self.assertFalse((directory / ".lampyr/dead-letter.jsonl").exists())

    @contract_for(native_only=True)
    def test_bodyless_responses_close_unread_sources(self):
        for runtime in RUNTIMES:
            with self.subTest(runtime=runtime), generated(runtime, fixture=True) as (directory, binary, extension):
                with serving(directory, binary, extension) as port:
                    for method, path, status in [("HEAD", "/close-head", 200),
                                                 ("GET", "/close-204", 204),
                                                 ("GET", "/close-304", 304)]:
                        result, _, body = call(port, path, method)
                        self.assertEqual(result, status)
                        self.assertEqual(body, b"")
                    expected = ["/close-head", "/close-204", "/close-304"]
                    closed = []
                    # Sources may be closed just after the response is flushed.
                    for _ in range(40):
                        closed = json.loads(call(port, "/closed-bodies")[2])
                        if len(closed) == len(expected):
                            break
                        time.sleep(0.05)
                    self.assertEqual(sorted(closed), sorted(expected))

    @contract_for("nodejs", "python")
    def test_kind_isolation_handlers_use_response_contract(self):
        source = (ROOT / "scripts/test-kind-integration.sh").read_text()
        cases = [
            ("nodejs", "$reuse_dir/handler.js", {"crypto", "uuid", "math"}),
            ("python", "$python_dir/handler.py", {"secrets", "uuid", "random", "urandom"}),
        ]
        for runtime, path, fields in cases:
            if runtime not in RUNTIMES:
                continue
            with self.subTest(runtime=runtime):
                marker = f"cat >\"{path}\" <<'EOF'\n"
                start = source.index(marker) + len(marker)
                handler = source[start:source.index("\nEOF", start)]
                with generated(runtime) as (directory, binary, extension):
                    (directory / handler_name(extension)).write_text(handler)
                    with serving(directory, binary, extension) as port:
                        first = json.loads(call(port, "/test")[2])
                        second = json.loads(call(port, "/test")[2])
                        self.assertEqual((first["count"], second["count"]), (1, 1) if VM else (1, 2))
                        self.assertEqual(set(first["random"]), fields)
                        self.assertTrue(all(first["random"][field] != second["random"][field] for field in fields))
                        self.assertEqual(call(port, "/events", "POST", "{}", EVENT_HEADERS)[0], 204)

    def test_responses_default_to_nosniff_unless_the_handler_sets_it(self):
        # Handler responses, streams and adapter-generated errors (framing headers
        # are adapter-owned, so /headers yields a 500).
        cases = [("GET", "/text", "nosniff"), ("POST", "/upload-stream", "nosniff"),
                 ("GET", "/headers", "nosniff"), ("GET", "/own-nosniff", "handler")]
        for runtime in RUNTIMES:
            with self.subTest(runtime=runtime), generated(runtime, fixture=True) as (directory, binary, extension):
                with serving(directory, binary, extension) as port:
                    for method, path, expected in cases:
                        connection = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
                        try:
                            connection.request(method, path, body=b"x" if method == "POST" else None, headers=host_headers())
                            response = connection.getresponse()
                            response.read()
                            self.assertEqual(response.headers.get_all("x-content-type-options"), [expected],
                                             (path, response.status))
                        finally:
                            connection.close()
                    # nosniff does not stop a server-side sniff: an untyped HTML stream
                    # must not come back labelled text/html.
                    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
                    try:
                        connection.request("POST", "/upload-stream", body=b"<html><script>alert(1)</script>", headers=host_headers())
                        response = connection.getresponse()
                        self.assertEqual((response.status, response.read()),
                                         (200, b"<html><script>alert(1)</script>"))
                        self.assertFalse(any(value.lower().startswith("text/html")
                                             for value in response.headers.get_all("content-type") or []),
                                         response.headers.get_all("content-type"))
                    finally:
                        connection.close()

    def test_native_hook_checks(self):
        for runtime in RUNTIMES:
            with self.subTest(runtime=runtime), generated(runtime, fixture=True) as (directory, binary, extension):
                source_snapshot = {path.relative_to(directory): path.read_bytes()
                                   for path in directory.rglob("*") if path.is_file()}
                command = native_command(directory, binary, extension) + ["--check"]
                env = {**os.environ, "PYTHONDONTWRITEBYTECODE": "1"}
                self.assertEqual(subprocess.run(command, cwd=directory, env=env, capture_output=True).returncode, 0)
                result = subprocess.run(command + ["--require-event"], cwd=directory, env=env, capture_output=True)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(b"events policy requires", result.stderr)
                self.assertEqual(source_snapshot, {
                    path.relative_to(directory): path.read_bytes()
                    for path in directory.rglob("*") if path.is_file()
                }, "native hook checks modified service source")
                manifest = directory / "lampyr.yaml"
                manifest.write_text(manifest.read_text() + (
                    "\nevents:\n  maxRetries: 1\n  initialBackoff: 1ms\n"
                    "  deadLetter: .lampyr/dead-letter.jsonl\n"))
                source_snapshot[Path("lampyr.yaml")] = manifest.read_bytes()
                with without_host_toolchains() as container_env:
                    result = subprocess.run([str(LY), "build", str(directory)],
                                            env=container_env, capture_output=True)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(b"events policy requires", result.stderr)
                self.assertFalse((directory / ".lampyr/build/rootfs.cpio").exists())
                self.assertFalse((directory / ".lampyr/artifact").exists())
                self.assertEqual(source_snapshot, {
                    path.relative_to(directory): path.read_bytes()
                    for path in directory.rglob("*")
                    if path.is_file() and ".lampyr" not in path.relative_to(directory).parts
                },
                    "container hook checks modified service source")
                handler = directory / handler_name(extension)
                if extension not in ("js", "py"):
                    # The compiler, not a source-pattern heuristic, enforces hook signatures.
                    handler.write_text("invalid native handler source\n")
                    with without_host_toolchains() as container_env:
                        result = subprocess.run([str(LY), "build", str(directory)],
                                                env=container_env, capture_output=True)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertNotIn(b"host language toolchain must not be invoked", result.stderr)
                    self.assertIn(b"Dockerfile.rootfs", result.stderr)
                    self.assertFalse((directory / ".lampyr/build/rootfs.cpio").exists())
                    continue
                with handler.open("a") as stream:
                    stream.write("\nmodule.exports.handleEvent = 1;\n" if extension == "js" else "\nhandle_event = 1\n")
                result = subprocess.run(command, cwd=directory, env=env, capture_output=True)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(b"must be", result.stderr)
                handler.write_text(
                    "module.exports = {};" if extension == "js" else "value = 1\n")
                tool_path = directory / "tools"
                tool_path.mkdir()
                os.symlink(shutil.which("docker"), tool_path / "docker")
                result = subprocess.run(
                    [str(LY), "build", "--verbose", str(directory)],
                    env={**os.environ, "PATH": str(tool_path)},
                    capture_output=True,
                )
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(b"required handle(request, context)", result.stderr)
                self.assertFalse((directory / ".lampyr/artifact").exists())
                self.assertFalse((directory / ".lampyr/build/rootfs.cpio").exists())

    @contract_for(native_only=True)
    def test_slow_reader_applies_backpressure(self):
        for runtime in RUNTIMES:
            with self.subTest(runtime=runtime), generated(runtime, fixture=True) as (directory, binary, extension):
                with serving(directory, binary, extension) as port:
                    with socket.create_connection(("127.0.0.1", port), timeout=5) as connection:
                        connection.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4096)
                        connection.sendall(b"GET /backpressure HTTP/1.1\r\nHost: service.default.localhost\r\n\r\n")
                        marker = directory / "produced"
                        deadline = time.monotonic() + 3
                        while not marker.exists() and time.monotonic() < deadline:
                            time.sleep(0.02)
                        self.assertTrue(marker.exists())

                        def produced():
                            # The fixture rewrites the marker, so a read can see it empty.
                            for _ in range(300):
                                text = marker.read_text().strip()
                                if text:
                                    return int(text)
                                time.sleep(0.01)
                            self.fail("producer marker stayed empty")

                        # Kernel send buffers grow under load, so let the producer
                        # settle (unchanged for 0.3 s) before asserting it stays put.
                        first, settle = produced(), time.monotonic() + 5
                        while time.monotonic() < settle:
                            time.sleep(0.3)
                            current = produced()
                            if current == first:
                                break
                            first = current
                        time.sleep(0.3)
                        self.assertEqual(produced(), first, "adapter kept consuming a blocked stream")
                        self.assertLess(first, 128, "adapter buffered the entire stream instead of applying backpressure")

    @contract_for(native_only=True)
    def test_request_body_limit(self):
        for runtime in RUNTIMES:
            with self.subTest(runtime=runtime), generated(runtime, fixture=True) as (directory, binary, extension):
                with serving(directory, binary, extension) as port:
                    self.assertEqual(call(port, "/echo", "POST", b"x" * (1024 * 1024))[0], 200)
                    self.assertEqual(oversized_status(port, "/echo", 1024 * 1024 + 1), 413)
                    if runtime == "nodejs":
                        with socket.create_connection(("127.0.0.1", port), timeout=5) as connection:
                            chunks = [b"x" * 65536 for _ in range(17)]
                            request = (b"POST /echo HTTP/1.1\r\nHost: service.default.localhost\r\n"
                                       b"Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n")
                            request += b"".join(
                                f"{len(chunk):x}\r\n".encode() + chunk + b"\r\n" for chunk in chunks)
                            request += b"0\r\n\r\n"
                            connection.sendall(request)
                            response = b""
                            while chunk := connection.recv(4096):
                                response += chunk
                        self.assertIn(b"HTTP/1.1 413", response)
                        self.assertIn(b"request body too large", response)

    def test_chunked_request_bodies(self):
        for runtime in RUNTIMES:
            with self.subTest(runtime=runtime), generated(runtime, fixture=True) as (directory, binary, extension):
                with serving(directory, binary, extension) as port:
                    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
                    try:
                        connection.request("POST", "/echo", body=iter([b"chunked ", b"request"]),
                                           headers=host_headers({"Content-Type": "application/octet-stream"}),
                                           encode_chunked=True)
                        response = connection.getresponse()
                        self.assertEqual(response.status, 200)
                        self.assertEqual(json.loads(response.read())["body"], "chunked request")
                    finally:
                        connection.close()

    def test_streaming_upload_before_request_completes(self):
        for runtime in RUNTIMES:
            with self.subTest(runtime=runtime), generated(runtime, fixture=True) as (directory, binary, extension):
                with serving(directory, binary, extension) as port:
                    with socket.create_connection(("127.0.0.1", port), timeout=5) as connection:
                        connection.sendall(
                            b"POST /upload-stream HTTP/1.1\r\nHost: service.default.localhost\r\n"
                            b"Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
                            b"6\r\nfirst!\r\n")
                        response = http.client.HTTPResponse(connection)
                        response.begin()
                        self.assertEqual(response.status, 200)
                        self.assertEqual(response.read(6), b"first!")
                        connection.sendall(b"6\r\nsecond\r\n0\r\n\r\n")
                        self.assertEqual(response.read(), b"second")
                        response.close()

    def test_repeated_headers_and_json_helpers(self):
        for runtime in RUNTIMES:
            with self.subTest(runtime=runtime), generated(runtime, fixture=True) as (directory, binary, extension):
                with serving(directory, binary, extension) as port:
                    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
                    try:
                        connection.putrequest("GET", "/repeated-headers", skip_host=VM)
                        if VM:
                            connection.putheader("Host", "service.default.localhost")
                        connection.putheader("X-Repeat", "one")
                        connection.putheader("x-repeat", "two")
                        connection.endheaders()
                        response = connection.getresponse()
                        self.assertEqual(response.status, 200)
                        self.assertEqual(response.headers.get_all("set-cookie"), ["a=1", "b=2"])
                        self.assertEqual(json.loads(response.read()), {"values": ["one", "two"]})
                    finally:
                        connection.close()
                    status, _, body = call(port, "/json-body", "POST", b'{"ok":true}')
                    self.assertEqual(status, 200)
                    self.assertEqual(json.loads(body), {"ok": True})
                    self.assertEqual(call(port, "/json-body", "POST", b"{")[0], 400)
                    self.assertEqual(call(port, "/json-body", "POST", b'"\xff"')[0], 400)
                    for invalid in (b"NaN", b"Infinity", b"-Infinity",
                                    '{"ok":true}'.encode("utf-16"), '{"ok":true}'.encode("utf-32")):
                        self.assertEqual(call(port, "/json-body", "POST", invalid)[0], 400)
                    self.assertEqual(call(port, "/echo", "POST", b"\xff")[0], 400)
                    if runtime in ("nodejs", "python"):
                        self.assertEqual(call(port, "/consume-twice", "POST", "payload")[0], 500)
                        self.assertEqual(call(port, "/read-limit", "POST", "1234")[2], b"1234")
                        self.assertEqual(call(port, "/read-limit", "POST", "12345")[0], 413)
                    self.assertEqual(json.loads(call(port, "/http-context")[2]), {"requestInContext": False})

    def test_incoming_body_failure_is_not_completion(self):
        for runtime in RUNTIMES:
            with self.subTest(runtime=runtime), generated(runtime, fixture=True) as (directory, binary, extension):
                with serving(directory, binary, extension) as port:
                    with socket.create_connection(("127.0.0.1", port), timeout=5) as connection:
                        connection.sendall(
                            b"POST /upload-stream HTTP/1.1\r\nHost: service.default.localhost\r\n"
                            b"Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
                            b"6\r\nfirst!\r\n")
                        response = http.client.HTTPResponse(connection)
                        response.begin()
                        self.assertEqual(response.status, 200)
                        self.assertEqual(response.read(6), b"first!")
                        connection.shutdown(socket.SHUT_WR)
                        with self.assertRaises((http.client.IncompleteRead, ConnectionResetError)):
                            response.read()
                        response.close()
                    self.assertEqual(call(port, "/value")[0], 201)

    @contract_for(native_only=True)
    def test_shutdown_interrupts_incomplete_upload(self):
        for runtime in RUNTIMES:
            with self.subTest(runtime=runtime), generated(runtime, fixture=True) as (directory, binary, extension):
                connection = None
                try:
                    with serving(directory, binary, extension) as port:
                        connection = socket.create_connection(("127.0.0.1", port), timeout=5)
                        connection.sendall(
                            b"POST /echo HTTP/1.1\r\nHost: service.default.localhost\r\n"
                            b"Transfer-Encoding: chunked\r\n\r\n1\r\nx\r\n")
                        time.sleep(0.1)
                finally:
                    if connection is not None:
                        connection.close()

    @contract_for(native_only=True)
    def test_shutdown_cancels_active_stream(self):
        for runtime in RUNTIMES:
            with self.subTest(runtime=runtime), generated(runtime, fixture=True) as (directory, binary, extension):
                with serving(directory, binary, extension) as port:
                    connection = socket.create_connection(("127.0.0.1", port), timeout=5)
                    connection.sendall(b"GET /cancel HTTP/1.1\r\nHost: service.default.localhost\r\n\r\n")
                    connection.recv(4096)
                    if runtime == "python":
                        deadline = time.monotonic() + 3
                        while not (directory / "sleeping").exists() and time.monotonic() < deadline:
                            time.sleep(0.01)
                        self.assertTrue((directory / "sleeping").exists(), "stream did not start sleeping")
                connection.close()
                self.assertTrue((directory / "cancelled").exists(), "shutdown did not close the active iterator")

    def test_websocket_fragmentation_and_coalescing(self):
        for runtime in RUNTIMES:
            with self.subTest(runtime=runtime), generated(runtime) as (directory, binary, extension):
                with serving(directory, binary, extension) as port:
                    self.check_websocket(port, runtime == "nodejs")

    @contract_for("python", native_only=True)
    def test_idle_python_websockets_do_not_starve_http(self):
        with generated("python") as (directory, binary, extension):
            with serving(directory, binary, extension) as port:
                connections = []
                try:
                    for index in range(9):
                        connection = socket.create_connection(("127.0.0.1", port), timeout=5)
                        connections.append(connection)
                        connection.sendall(
                            b"GET /ws HTTP/1.1\r\nHost: service.default.localhost\r\nUpgrade: websocket\r\n"
                            b"Connection: Upgrade\r\nSec-WebSocket-Version: 13\r\n"
                            b"Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n")
                        response = b""
                        while b"\r\n\r\n" not in response:
                            chunk = connection.recv(4096)
                            self.assertTrue(chunk, "connection closed before upgrade response")
                            response += chunk
                        expected = b"101 Switching Protocols" if index < 8 else b"503 Service Unavailable"
                        self.assertIn(expected, response)
                    self.assertEqual(call(port, "/hello")[0], 200)
                    status, _, body = call(port, "/events", "POST", "{}", EVENT_HEADERS)
                    self.assertEqual(status, 200)
                    self.assertEqual(json.loads(body)["event"]["id"], "contract-id")
                finally:
                    for connection in connections:
                        connection.close()

    def upgrade_websocket(self, connection, path):
        connection.sendall(
            f"GET {path} HTTP/1.1\r\nHost: service.default.localhost\r\nUpgrade: websocket\r\n"
            "Connection: Upgrade\r\nSec-WebSocket-Version: 13\r\n"
            "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n".encode())
        response = b""
        while b"\r\n\r\n" not in response:
            chunk = connection.recv(4096)
            self.assertTrue(chunk, "connection closed before upgrade")
            response += chunk
        headers, frame = response.split(b"\r\n\r\n", 1)
        self.assertIn(b"101 Switching Protocols", headers)
        return frame

    def assert_websocket_close(self, connection, frame, code, reason):
        started = time.monotonic()
        try:
            while chunk := connection.recv(4096):
                frame += chunk
        except ConnectionResetError:
            pass
        self.assertLess(time.monotonic() - started, 3, "WebSocket close was not bounded")
        payload = struct.pack("!H", code) + reason.encode()
        self.assertEqual(frame, bytes([0x88, len(payload)]) + payload)

    def check_websocket(self, port, many_empty_continuations):
        with self.subTest(port=port):
            with socket.create_connection(("127.0.0.1", port), timeout=5) as connection:
                connection.sendall(
                    b"GET /ws HTTP/1.1\r\nHost: service.default.localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
                    b"Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n")
                self.assertIn(b"101 Switching Protocols", connection.recv(4096))
                empty_continuations = websocket_frame(0, b"", False) * 5000 if many_empty_continuations else b""
                message = websocket_frame(1, b"hello", False) + empty_continuations + websocket_frame(0, b" world")
                connection.sendall(message[:3])
                connection.sendall(message[3:] + websocket_frame(9, b"ping"))
                def exact(length):
                    output = b""
                    while len(output) < length:
                        chunk = connection.recv(length - len(output))
                        self.assertTrue(chunk, "WebSocket closed before complete frame")
                        output += chunk
                    return output
                messages = []
                for _ in range(2):
                    opcode, length = exact(2)
                    messages.append((opcode, exact(length)))
                self.assertCountEqual(messages, [(0x81, b"hello world"), (0x8a, b"ping")])
                connection.sendall(websocket_frame(2, b"x" * 300))
                self.assertEqual(exact(304), b"\x82\x7e\x01\x2c" + b"x" * 300)
                for _ in range(5):
                    connection.sendall(websocket_frame(1, b"still active"))
                    self.assertEqual(exact(14), b"\x81\x0cstill active")
                    time.sleep(0.06)
                connection.sendall(websocket_frame(8, b"\x03\xe8"))
                self.assertEqual(exact(4), b"\x88\x02\x03\xe8")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--ly", type=Path, default=LY)
    parser.add_argument("--vm", action="store_true", help="run generated services through ly dev in Hyperlight")
    parser.add_argument("--runtime", action="append", choices=RUNTIMES, help="restrict to selected runtimes")
    arguments, remaining = parser.parse_known_args()
    LY = arguments.ly.resolve()
    VM = arguments.vm
    if arguments.runtime:
        RUNTIMES = {runtime: RUNTIMES[runtime] for runtime in arguments.runtime}
    with tempfile.TemporaryDirectory() as native_cache:
        NATIVE_CACHE = Path(native_cache)
        unittest.main(argv=[__file__, *remaining], testLoader=ContractLoader())
