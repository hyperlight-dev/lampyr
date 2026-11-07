#!/usr/bin/env python3
"""Check an example's WebSocket echo, ping/pong, and close handshake."""

import base64
import hashlib
import os
import socket
import ssl
import sys


port, service = int(sys.argv[1]), sys.argv[2]
host = f"{service}.default.localhost"
context = ssl.create_default_context()
context.check_hostname = False
context.verify_mode = ssl.CERT_NONE
with context.wrap_socket(
    socket.create_connection(("127.0.0.1", port), timeout=15),
    server_hostname=host,
) as connection:
    key = base64.b64encode(os.urandom(16)).decode()
    connection.sendall(
        (
            f"GET /ws HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\n"
            f"Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\n"
            "Sec-WebSocket-Version: 13\r\n\r\n"
        ).encode()
    )
    response = b""
    while b"\r\n\r\n" not in response:
        chunk = connection.recv(1)
        if not chunk or len(response) >= 65536:
            raise SystemExit("invalid WebSocket upgrade response")
        response += chunk
    if not response.startswith(b"HTTP/1.1 101"):
        raise SystemExit(f"{service} WebSocket upgrade failed: {response!r}")
    accept = base64.b64encode(
        hashlib.sha1((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()
    )
    headers = dict(line.split(b":", 1) for line in response.split(b"\r\n")[1:] if b":" in line)
    if {name.lower(): value.strip() for name, value in headers.items()}.get(b"sec-websocket-accept") != accept:
        raise SystemExit("invalid Sec-WebSocket-Accept header")

    def send(opcode, payload=b""):
        mask = os.urandom(4)
        masked = bytes(value ^ mask[index % 4] for index, value in enumerate(payload))
        connection.sendall(bytes([0x80 | opcode, 0x80 | len(payload)]) + mask + masked)

    def receive_exact(size):
        data = b""
        while len(data) < size:
            chunk = connection.recv(size - len(data))
            if not chunk:
                raise SystemExit("incomplete WebSocket frame")
            data += chunk
        return data

    def receive():
        header = receive_exact(2)
        length = header[1] & 0x7F
        if length == 126:
            length = int.from_bytes(receive_exact(2), "big")
        elif length == 127:
            length = int.from_bytes(receive_exact(8), "big")
        if length > 65536:
            raise SystemExit("unexpectedly large WebSocket frame")
        return header[0] & 0x0F, receive_exact(length)

    send(0x1, b"lampyr")
    if receive() != (0x1, b"lampyr"):
        raise SystemExit("WebSocket text echo failed")
    send(0x9, b"ping")
    if receive() != (0xA, b"ping"):
        raise SystemExit("WebSocket ping/pong failed")
    send(0x8)
    if receive()[0] != 0x8:
        raise SystemExit("WebSocket close handshake failed")
