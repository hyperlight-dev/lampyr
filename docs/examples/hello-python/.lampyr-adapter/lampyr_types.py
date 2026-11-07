import json
from dataclasses import dataclass, field
from threading import Event as Cancellation
from typing import Iterable, Iterator, Protocol, TypeAlias, Union

Value: TypeAlias = Union[None, bool, int, float, str, list["Value"], dict[str, "Value"]]


class HandlerError(Exception):
    def __init__(self, status: int, message: str):
        if not 400 <= status <= 599:
            raise ValueError("HandlerError status must be between 400 and 599")
        super().__init__(message)
        self.status = status


class Headers:
    def __init__(self, entries: Iterable[tuple[str, str]] = ()):
        self._entries = [(name.lower(), value) for name, value in entries]

    def get(self, name: str, default: str = "") -> str:
        return next((value for key, value in self if key == name.lower()), default)

    def get_all(self, name: str) -> list[str]:
        return [value for key, value in self if key == name.lower()]

    def append(self, name: str, value: str) -> None:
        self._entries.append((name.lower(), value))

    def set(self, name: str, value: str) -> None:
        self._entries = [(key, item) for key, item in self if key != name.lower()]
        self.append(name, value)

    def __iter__(self) -> Iterator[tuple[str, str]]:
        return iter(self._entries)


class Body:
    def __init__(self, source: Iterable[bytes] = (), limit: int | None = None):
        self._source = source
        self._limit = limit
        self._used = False
        self._closed = False
        self._iterator: Iterator[bytes] | None = None

    def __iter__(self) -> Iterator[bytes]:
        if self._used:
            raise RuntimeError("body has already been consumed")
        self._used = True
        self._iterator = iter(self._source)
        size = 0
        try:
            for chunk in self._iterator:
                if not isinstance(chunk, bytes):
                    raise TypeError("body chunks must be bytes")
                size += len(chunk)
                if self._limit is not None and size > self._limit:
                    raise HandlerError(413, "request body too large")
                yield chunk
        finally:
            self.close()

    def close(self) -> None:
        if self._closed:
            return
        self._closed = True
        iterator, self._iterator = self._iterator if self._iterator is not None else self._source, None
        self._used = True
        if iterator is not None and hasattr(iterator, "close"):
            iterator.close()

    def bytes(self, limit: int = 1024 * 1024) -> bytes:
        chunks = []
        size = 0
        for chunk in self:
            size += len(chunk)
            if size > limit:
                raise HandlerError(413, "body read limit exceeded")
            chunks.append(chunk)
        return b"".join(chunks)

    def text(self, limit: int = 1024 * 1024) -> str:
        try:
            return self.bytes(limit).decode()
        except UnicodeDecodeError as error:
            raise HandlerError(400, "invalid UTF-8 body") from error

    def json(self, limit: int = 1024 * 1024) -> Value:
        def reject_constant(value):
            raise ValueError(f"invalid JSON constant: {value}")

        try:
            return json.loads(self.text(limit), parse_constant=reject_constant)
        except (ValueError, UnicodeDecodeError) as error:
            raise HandlerError(400, "invalid JSON body") from error


@dataclass
class RequestHead:
    method: str
    path: str
    headers: Headers


@dataclass
class Request(RequestHead):
    body: Body


@dataclass
class Context:
    cancelled: Cancellation


@dataclass
class ProtocolContext(Context):
    request: RequestHead


@dataclass
class Response:
    body: Body = field(default_factory=Body)
    status: int = 200
    headers: Headers = field(default_factory=Headers)

    @classmethod
    def bytes(cls, value: bytes, *, status: int = 200, headers: Headers | None = None) -> "Response":
        return cls(Body([value]), status, headers if headers is not None else Headers())

    @classmethod
    def text(cls, value: str, *, status: int = 200, headers: Headers | None = None) -> "Response":
        fields = Headers(headers) if headers is not None else Headers()
        if not fields.get("content-type"):
            fields.set("content-type", "text/plain; charset=utf-8")
        return cls.bytes(value.encode(), status=status, headers=fields)

    @classmethod
    def json(cls, value: Value, *, status: int = 200, headers: Headers | None = None) -> "Response":
        fields = Headers(headers) if headers is not None else Headers()
        if not fields.get("content-type"):
            fields.set("content-type", "application/json")
        return cls.bytes(json.dumps(value, separators=(",", ":")).encode(), status=status, headers=fields)


@dataclass
class Event:
    id: str
    type: str
    source: str
    subject: str = ""
    time: str = ""
    attempt: int = 1
    data: Value = None


class WebSocketSession(Protocol):
    def __iter__(self) -> Iterator[str | bytes]: ...
    def send(self, data: str | bytes) -> None: ...
    def close(self, code: int = 1000, reason: str = "") -> None: ...
