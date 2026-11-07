#[path = "../handler.rs"]
mod handler;
#[allow(dead_code)]
mod types;

use std::{convert::Infallible, panic::AssertUnwindSafe, sync::Arc};

use futures_util::{stream, FutureExt, StreamExt};
use http::{header, HeaderMap, StatusCode};
use http_body_util::{combinators::UnsyncBoxBody, BodyExt, Limited, StreamBody};
use hyper::{body::Incoming, server::conn::http1, service::service_fn};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde_json::Value;
use tokio::net::TcpListener;
use tokio_tungstenite::{
    tungstenite::{
        handshake::server::create_response,
        protocol::{frame::coding::CloseCode, Role, WebSocketConfig},
    },
    WebSocketStream,
};
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use types::{
    Body, BodyStream, BoxError, Bytes, Context, Event, HandlerError, Hooks, ProtocolContext,
    Request, RequestHead, Response, WebSocketSession,
};

const LIMIT: usize = 1024 * 1024;
type WireBody = UnsyncBoxBody<Bytes, BoxError>;
type WireResponse = hyper::Response<WireBody>;

struct CancelOnDrop(CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

fn error(status: u16, message: impl Into<String>) -> BoxError {
    Box::new(HandlerError::new(status, message))
}

fn header_text<'a>(headers: &'a HeaderMap, key: &str) -> Result<Option<&'a str>, BoxError> {
    let mut values = headers.get_all(key).iter();
    let value = values
        .next()
        .map(|value| value.to_str())
        .transpose()
        .map_err(|_| error(400, "invalid event header"))?;
    if values.next().is_some() {
        return Err(error(400, "duplicate event header"));
    }
    Ok(value)
}

fn media_type(value: &str) -> String {
    value
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

fn event_media(request: &RequestHead) -> Result<Option<String>, BoxError> {
    // Classify leniently so ordinary HTTP requests never hit event-header
    // validation; strict checks apply only once a request is an event.
    let binary = request
        .headers
        .keys()
        .any(|key| key.as_str().starts_with("ce-"));
    let structured = request
        .headers
        .get_all(header::CONTENT_TYPE)
        .iter()
        .any(|value| {
            media_type(&String::from_utf8_lossy(value.as_bytes())) == "application/cloudevents+json"
        });
    if !binary && !structured {
        return Ok(None);
    }
    Ok(Some(media_type(
        header_text(&request.headers, "content-type")?.unwrap_or(""),
    )))
}

fn parse_event(request: &RequestHead, body: &[u8]) -> Result<Option<Event>, BoxError> {
    let Some(media) = event_media(request)? else {
        return Ok(None);
    };
    let structured = media == "application/cloudevents+json";
    let attempt = match header_text(&request.headers, "x-lampyr-delivery-attempt")? {
        None => 1,
        Some(value) if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) => {
            value
                .parse::<u64>()
                .ok()
                .filter(|value| *value > 0)
                .ok_or_else(|| error(400, "invalid delivery attempt"))?
        }
        _ => return Err(error(400, "invalid delivery attempt")),
    };
    let (version, id, event_type, source, subject, time, data) = if structured {
        let value: Value =
            serde_json::from_slice(body).map_err(|_| error(400, "invalid CloudEvent JSON"))?;
        let fields = value
            .as_object()
            .ok_or_else(|| error(400, "invalid CloudEvent object"))?;
        let required = |key: &str| -> Result<String, BoxError> {
            fields
                .get(key)
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .map(str::to_owned)
                .ok_or_else(|| error(400, format!("invalid CloudEvent {key}")))
        };
        let optional = |key: &str| -> Result<String, BoxError> {
            match fields.get(key) {
                None => Ok(String::new()),
                Some(Value::String(value)) => Ok(value.clone()),
                _ => Err(error(400, format!("invalid CloudEvent {key}"))),
            }
        };
        // The native contract exposes JSON values or text, not an ambiguous
        // base64 payload that could silently be mistaken for an absent body.
        if fields.contains_key("data_base64") {
            return Err(error(400, "data_base64 is not supported; use binary HTTP"));
        }
        (
            required("specversion")?,
            required("id")?,
            required("type")?,
            required("source")?,
            optional("subject")?,
            optional("time")?,
            fields.get("data").cloned().unwrap_or(Value::Null),
        )
    } else {
        let required = |key| -> Result<String, BoxError> {
            header_text(&request.headers, key)?
                .filter(|s| !s.trim().is_empty())
                .map(str::to_owned)
                .ok_or_else(|| error(400, format!("missing {key}")))
        };
        let text = std::str::from_utf8(body).map_err(|_| error(400, "event data must be UTF-8"))?;
        let data = serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_owned()));
        (
            required("ce-specversion")?,
            required("ce-id")?,
            required("ce-type")?,
            required("ce-source")?,
            header_text(&request.headers, "ce-subject")?
                .unwrap_or("")
                .to_owned(),
            header_text(&request.headers, "ce-time")?
                .unwrap_or("")
                .to_owned(),
            data,
        )
    };
    if version != "1.0" {
        return Err(error(400, "CloudEvent specversion must be 1.0"));
    }
    Ok(Some(Event {
        id,
        event_type,
        source,
        subject,
        time,
        data,
        attempt,
    }))
}

fn response_error(failure: BoxError) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        header::HeaderValue::from_static("nosniff"),
    );
    if let Some(typed) = failure.downcast_ref::<HandlerError>() {
        if (400..=599).contains(&typed.status) {
            return Response {
                status: typed.status,
                headers,
                ..Response::bytes(typed.message.clone())
            };
        }
    }
    eprintln!("handler error: {failure}");
    Response {
        status: 500,
        headers,
        ..Response::bytes("Internal Server Error")
    }
}

fn prepare(response: Response, head: bool, guard: CancelOnDrop) -> Result<WireResponse, BoxError> {
    if !(200..=599).contains(&response.status) {
        return Err(error(500, "Internal Server Error"));
    }
    if response
        .headers
        .values()
        .any(|value| value.to_str().is_err())
    {
        eprintln!("handler supplied an invalid response header value");
        return Err(error(500, "Internal Server Error"));
    }
    for key in ["content-length", "transfer-encoding", "connection"] {
        if response.headers.contains_key(key) {
            eprintln!("handler supplied adapter-owned response header: {key}");
            return Err(error(500, "Internal Server Error"));
        }
    }
    let mut headers = response.headers;
    // Browsers must not sniff handler bytes into HTML; a handler may set its own.
    if !headers.contains_key(header::X_CONTENT_TYPE_OPTIONS) {
        headers.insert(
            header::X_CONTENT_TYPE_OPTIONS,
            header::HeaderValue::from_static("nosniff"),
        );
    }
    let stream = response.body.0;
    // Drop a suppressed stream without ever polling it.
    let stream = if head || matches!(response.status, 204 | 304) {
        drop(stream);
        Box::pin(stream::empty()) as BodyStream
    } else {
        stream
    };
    let token = guard.0.clone();
    let frames = stream::unfold((stream, guard), move |(mut stream, guard)| {
        let token = token.clone();
        async move {
            let next = tokio::select! {
                biased;
                _ = token.cancelled() => Some(Err(error(500, "request cancelled"))),
                value = stream.next() => value,
            };
            next.map(|chunk| {
                let frame = chunk.map(hyper::body::Frame::data).map_err(|failure| {
                    eprintln!("response stream error: {failure}");
                    failure
                });
                (frame, (stream, guard))
            })
        }
    });
    let mut wire = hyper::Response::new(StreamBody::new(frames).boxed_unsync());
    *wire.status_mut() = StatusCode::from_u16(response.status)?;
    *wire.headers_mut() = headers;
    Ok(wire)
}

async fn dispatch(
    mut incoming: hyper::Request<Incoming>,
    hooks: Arc<Hooks>,
    connection: CancellationToken,
    shutdown: CancellationToken,
    tasks: TaskTracker,
) -> Result<WireResponse, BoxError> {
    let token = connection.child_token();
    let guard = CancelOnDrop(token.clone());
    let head = incoming.method() == http::Method::HEAD;
    let upgrade = incoming
        .headers()
        .get(header::UPGRADE)
        .is_some_and(|value| value.as_bytes().eq_ignore_ascii_case(b"websocket"));
    let upgrading = if upgrade {
        Some(hyper::upgrade::on(&mut incoming))
    } else {
        None
    };
    let (parts, body) = incoming.into_parts();
    if parts
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length > LIMIT as u64)
    {
        return Err(error(413, "request body exceeds 1 MiB"));
    }
    let head_request = RequestHead {
        method: parts.method.to_string(),
        path: parts
            .uri
            .path_and_query()
            .map_or("/", |path| path.as_str())
            .to_owned(),
        headers: parts.headers,
    };
    let body_token = token.clone();
    let chunks = Limited::new(body, LIMIT).into_data_stream();
    let body = Body::stream(stream::unfold(
        Some((chunks, body_token)),
        |state| async move {
            let (mut chunks, token) = state?;
            let chunk = tokio::select! {
                _ = token.cancelled() => return Some((
                    Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "request cancelled").into()),
                    None,
                )),
                chunk = chunks.next() => chunk,
            };
            chunk.map(|result| {
                let result = result.map_err(|failure| {
                    if failure.is::<http_body_util::LengthLimitError>() {
                        error(413, "request body exceeds 1 MiB")
                    } else {
                        error(400, "invalid request body")
                    }
                });
                (result, Some((chunks, token)))
            })
        },
    ));
    let mut request = Request {
        head: head_request,
        body,
    };
    let context = Context { cancelled: token };
    if hooks.event.is_none() && event_media(&request)?.is_some() {
        return Err(error(404, "event hook is not supported"));
    }
    let response = if event_media(&request)?.is_some() {
        let body = request.body.read_all().await?;
        let event = parse_event(&request, &body)?.ok_or_else(|| error(400, "invalid event"))?;
        match &hooks.event {
            Some(handle) => {
                handle(
                    event,
                    ProtocolContext {
                        request: request.head,
                        cancelled: context.cancelled,
                    },
                )
                .await?
            }
            None => Response {
                status: 404,
                ..Response::default()
            },
        }
    } else if let Some(upgrading) = upgrading {
        let Some(handle) = hooks.websocket.clone() else {
            return prepare(
                Response {
                    status: 404,
                    ..Response::default()
                },
                head,
                guard,
            );
        };
        let mut handshake = http::Request::new(());
        *handshake.method_mut() = parts.method;
        *handshake.uri_mut() = parts.uri;
        *handshake.version_mut() = parts.version;
        *handshake.headers_mut() = request.headers.clone();
        let reply =
            create_response(&handshake).map_err(|_| error(400, "invalid WebSocket upgrade"))?;
        let token = shutdown.child_token();
        let websocket_context = ProtocolContext {
            request: request.head,
            cancelled: token.clone(),
        };
        tasks.spawn(async move {
            let _guard = CancelOnDrop(token.clone());
            let transport = tokio::select! {
                _ = token.cancelled() => return,
                transport = upgrading => match transport {
                    Ok(transport) => transport,
                    Err(error) => {
                        eprintln!("WebSocket upgrade error: {error}");
                        return;
                    }
                },
            };
            let config = WebSocketConfig::default()
                .max_message_size(Some(LIMIT))
                .max_frame_size(Some(LIMIT));
            let socket = WebSocketStream::from_raw_socket(
                TokioIo::new(transport),
                Role::Server,
                Some(config),
            )
            .await;
            let (session, driver) = WebSocketSession::start(socket, token.clone());
            let operation = async { handle(session, websocket_context).await };
            let (code, reason) = tokio::select! {
                biased;
                result = AssertUnwindSafe(operation).catch_unwind() => match result {
                    Ok(Ok(())) => (CloseCode::Normal, ""),
                    Ok(Err(failure)) => {
                        eprintln!("WebSocket handler error: {failure}");
                        (CloseCode::Error, "request failed")
                    }
                    Err(_) => {
                        eprintln!("WebSocket handler panicked");
                        (CloseCode::Error, "request failed")
                    }
                },
                _ = token.cancelled() => (CloseCode::Normal, ""),
            };
            let (code, reason) = if shutdown.is_cancelled() {
                (CloseCode::Away, "server shutdown")
            } else {
                (code, reason)
            };
            driver.finish(code, reason).await;
        });
        drop(guard);
        let (parts, ()) = reply.into_parts();
        return Ok(hyper::Response::from_parts(
            parts,
            http_body_util::Empty::new()
                .map_err(|never: Infallible| match never {})
                .boxed_unsync(),
        ));
    } else {
        (hooks.handle)(request, context).await?
    };
    prepare(response, head, guard)
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = term.recv() => {},
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    let hooks = Arc::new(handler::hooks());
    let args: Vec<_> = std::env::args().skip(1).collect();
    let check = args.iter().any(|arg| arg == "--check");
    if args
        .iter()
        .any(|arg| arg != "--check" && arg != "--require-event")
        || (!check && !args.is_empty())
    {
        return Err("usage: lampyr-service [--check [--require-event]]".into());
    }
    if args.iter().any(|arg| arg == "--require-event") && hooks.event.is_none() {
        eprintln!("events policy requires handle_event(event, context) registration");
        std::process::exit(1);
    }
    if check {
        return Ok(());
    }
    let port: u16 = std::env::var("PORT")
        .unwrap_or_else(|_| "8080".into())
        .parse()?;
    let host = std::env::var("LAMPYR_LISTEN_HOST").unwrap_or_else(|_| "0.0.0.0".into());
    let listener = TcpListener::bind((host.as_str(), port)).await?;
    let shutdown = CancellationToken::new();
    let tasks = TaskTracker::new();
    // Register signal listeners once: re-creating them per iteration drops a
    // SIGTERM that arrives while a connection is being accepted.
    let stop_signal = shutdown_signal();
    tokio::pin!(stop_signal);
    loop {
        tokio::select! {
            biased;
            _ = &mut stop_signal => break,
            accepted = listener.accept() => {
                let socket = match accepted {
                    Ok((socket, _)) => socket,
                    Err(failure) => {
                        // Accept failures such as EMFILE are transient; exiting
                        // here would drop every in-flight request.
                        eprintln!("accept error: {failure}");
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        continue;
                    }
                };
                let _ = socket.set_nodelay(true);
                let connection = shutdown.child_token();
                let guard = CancelOnDrop(connection.clone());
                let hooks = hooks.clone();
                let shutdown = shutdown.clone();
                let children = tasks.clone();
                tasks.spawn(async move {
                    let _guard = guard;
                    let stop = connection.clone();
                    let service = service_fn(move |request| {
                        let head = request.method() == http::Method::HEAD;
                        let token = connection.child_token();
                        let fallback_token = token.clone();
                        let future = dispatch(request, hooks.clone(), token, shutdown.clone(), children.clone());
                        async move {
                            let response = match AssertUnwindSafe(future).catch_unwind().await {
                                Ok(Ok(response)) => response,
                                result => {
                                    let failure = match result {
                                        Ok(Err(failure)) => failure,
                                        Err(_) => "handler panicked".into(),
                                        _ => unreachable!(),
                                    };
                                    prepare(response_error(failure), head, CancelOnDrop(fallback_token))
                                        .expect("adapter error response is valid")
                                }
                            };
                            Ok::<_, Infallible>(response)
                        }
                    });
                    tokio::select! {
                        _ = stop.cancelled() => {},
                        // Without a timer hyper disables its 30 s header-read timeout,
                        // so idle connections would hold descriptors indefinitely.
                        result = http1::Builder::new().timer(TokioTimer::new()).serve_connection(TokioIo::new(socket), service).with_upgrades() => {
                            if let Err(failure) = result {
                                eprintln!("HTTP connection error: {failure}");
                            }
                        }
                    }
                });
            }
        }
    }
    shutdown.cancel();
    tasks.close();
    // Cancellation drops active bodies and handler futures before process exit.
    tasks.wait().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn event_request() -> RequestHead {
        RequestHead {
            method: "POST".into(),
            path: "/not-an-adapter-route?query=original".into(),
            headers: HeaderMap::from_iter([
                ("ce-specversion".parse().unwrap(), "1.0".parse().unwrap()),
                ("ce-id".parse().unwrap(), "event-id".parse().unwrap()),
                (
                    "ce-type".parse().unwrap(),
                    "example.created".parse().unwrap(),
                ),
                ("ce-source".parse().unwrap(), "/contract".parse().unwrap()),
                (
                    "content-type".parse().unwrap(),
                    "application/json".parse().unwrap(),
                ),
            ]),
        }
    }

    #[test]
    fn binary_and_structured_events_have_the_same_native_shape() {
        let binary = parse_event(&event_request(), b"{\"ok\":true}")
            .unwrap()
            .unwrap();
        let mut structured = event_request();
        structured.headers.clear();
        structured.headers.insert(
            header::CONTENT_TYPE,
            "application/cloudevents+json; charset=utf-8"
                .parse()
                .unwrap(),
        );
        let body = serde_json::to_vec(&json!({
            "specversion": "1.0", "id": "event-id", "type": "example.created",
            "source": "/contract", "data": {"ok": true},
        }))
        .unwrap();
        let structured = parse_event(&structured, &body).unwrap().unwrap();
        assert_eq!(
            serde_json::to_value(binary).unwrap(),
            serde_json::to_value(structured).unwrap()
        );
    }

    #[test]
    fn malformed_event_attributes_are_rejected() {
        for key in ["ce-specversion", "ce-id", "ce-type", "ce-source"] {
            let mut request = event_request();
            request.headers.remove(key);
            assert!(parse_event(&request, b"{}").is_err(), "{key}");
        }
        for value in ["0", "-1", "+1", "1.5", "invalid", "18446744073709551616"] {
            let mut request = event_request();
            request
                .headers
                .insert("x-lampyr-delivery-attempt", value.parse().unwrap());
            assert!(parse_event(&request, b"{}").is_err(), "{value}");
        }
        let mut request = event_request();
        request
            .headers
            .insert("ce-specversion", "0.3".parse().unwrap());
        assert!(parse_event(&request, b"{}").is_err());
        request
            .headers
            .insert("ce-specversion", "1.0".parse().unwrap());
        request
            .headers
            .append("ce-id", "duplicate".parse().unwrap());
        assert!(parse_event(&request, b"{}").is_err());
    }

    #[test]
    fn response_status_and_framing_are_owned_by_the_adapter() {
        for status in [0, 100, 199, 600, 999] {
            let response = Response {
                status,
                ..Response::default()
            };
            assert!(prepare(response, false, CancelOnDrop(CancellationToken::new())).is_err());
        }
        for key in ["content-length", "transfer-encoding", "connection"] {
            let mut response = Response::default();
            response.headers.insert(
                http::header::HeaderName::from_static(key),
                "bad".parse().unwrap(),
            );
            assert!(prepare(response, false, CancelOnDrop(CancellationToken::new())).is_err());
        }
    }

    #[tokio::test]
    async fn suppressed_streams_are_dropped_without_polling() {
        for (head, status) in [(true, 200), (false, 204), (false, 304)] {
            let polled = Arc::new(AtomicBool::new(false));
            let observed = polled.clone();
            let stream = stream::poll_fn(move |_| {
                observed.store(true, Ordering::SeqCst);
                std::task::Poll::Ready(Some(Ok(Bytes::from_static(b"must not appear"))))
            });
            let response = Response {
                status,
                ..Response::stream(stream)
            };
            let body = prepare(response, head, CancelOnDrop(CancellationToken::new()))
                .unwrap()
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes();
            assert!(body.is_empty());
            assert!(!polled.load(Ordering::SeqCst));
        }
    }

    #[tokio::test]
    async fn dropping_response_cancels_context_and_drops_stream() {
        struct Cleanup(Arc<AtomicBool>);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let closed = Arc::new(AtomicBool::new(false));
        let cleanup = Cleanup(closed.clone());
        let stream = stream::unfold(cleanup, |cleanup| async move {
            Some((Ok(Bytes::from_static(b"chunk")), cleanup))
        });
        let token = CancellationToken::new();
        let response =
            prepare(Response::stream(stream), false, CancelOnDrop(token.clone())).unwrap();
        drop(response);
        assert!(token.is_cancelled());
        assert!(closed.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn unexpected_errors_do_not_expose_details() {
        let mut response = response_error(std::io::Error::other("private details").into());
        assert_eq!(response.status, 500);
        assert_eq!(
            response.body.read_all().await.unwrap(),
            "Internal Server Error"
        );
        assert_eq!(
            response_error(HandlerError::new(422, "invalid input").into()).status,
            422
        );
        assert_eq!(
            response_error(HandlerError::new(200, "invalid status").into()).status,
            500
        );
    }

    #[tokio::test]
    async fn bounded_helpers_preserve_stream_failures() {
        let mut body = Body::bytes(vec![0; LIMIT]);
        assert_eq!(body.read_all().await.unwrap().len(), LIMIT);
        let mut body = Body::bytes(vec![0; LIMIT + 1]);
        assert_eq!(
            body.read_all()
                .await
                .unwrap_err()
                .downcast_ref::<HandlerError>()
                .unwrap()
                .status,
            413,
        );
        let mut body = Body::stream(stream::once(async {
            Err(std::io::Error::other("transport failed").into())
        }));
        let failure = body.read_all().await.unwrap_err();
        assert!(failure.downcast_ref::<std::io::Error>().is_some());
        let mut body = Body::bytes("{");
        assert_eq!(
            body.json::<Value>()
                .await
                .unwrap_err()
                .downcast_ref::<HandlerError>()
                .unwrap()
                .status,
            400,
        );
    }
}
