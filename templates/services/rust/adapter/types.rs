//! Native handler contract. This module is compiled together with ../handler.rs.
use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

pub use bytes::Bytes;
use futures_util::{future::BoxFuture, stream::SplitSink, SinkExt, Stream, StreamExt};
pub use http::HeaderMap;
use hyper::upgrade::Upgraded;
use hyper_util::rt::TokioIo;
use serde::{de::DeserializeOwned, Serialize};
use tokio::{sync::Mutex, task::JoinHandle};
pub use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::{
    protocol::{frame::coding::CloseCode, CloseFrame},
    Error as WebSocketError,
};
use tokio_tungstenite::WebSocketStream;
pub use tokio_util::sync::CancellationToken;

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub type HandlerResult = Result<Response, BoxError>;
pub type BodyStream = Pin<Box<dyn Stream<Item = Result<Bytes, BoxError>> + Send>>;

#[derive(Clone, Debug)]
pub struct RequestHead {
    pub method: String,
    /// Original request target, including the query string.
    pub path: String,
    pub headers: HeaderMap,
}

pub struct Request {
    pub head: RequestHead,
    pub body: Body,
}

impl std::ops::Deref for Request {
    type Target = RequestHead;
    fn deref(&self) -> &Self::Target {
        &self.head
    }
}

#[derive(Clone)]
pub struct Context {
    pub cancelled: CancellationToken,
}

#[derive(Clone)]
pub struct ProtocolContext {
    pub request: RequestHead,
    pub cancelled: CancellationToken,
}

#[derive(Debug)]
pub struct HandlerError {
    pub status: u16,
    pub message: String,
}

impl HandlerError {
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for HandlerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for HandlerError {}

pub struct Body(pub(crate) BodyStream);

impl Body {
    pub fn stream(stream: impl Stream<Item = Result<Bytes, BoxError>> + Send + 'static) -> Self {
        Self(Box::pin(stream))
    }

    pub fn bytes(value: impl Into<Bytes>) -> Self {
        let bytes = value.into();
        Self::stream(futures_util::stream::once(async { Ok(bytes) }))
    }

    pub async fn read_all(&mut self) -> Result<Bytes, BoxError> {
        let mut bytes = bytes::BytesMut::new();
        while let Some(chunk) = self.0.next().await {
            let chunk = chunk?;
            if bytes.len() + chunk.len() > 1024 * 1024 {
                return Err(HandlerError::new(413, "body exceeds 1 MiB").into());
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes.freeze())
    }

    pub async fn text(&mut self) -> Result<String, BoxError> {
        String::from_utf8(self.read_all().await?.to_vec())
            .map_err(|_| HandlerError::new(400, "invalid UTF-8 body").into())
    }

    pub async fn json<T: DeserializeOwned>(&mut self) -> Result<T, BoxError> {
        serde_json::from_slice(&self.read_all().await?)
            .map_err(|_| HandlerError::new(400, "invalid JSON body").into())
    }
}

impl Stream for Body {
    type Item = Result<Bytes, BoxError>;
    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.0.as_mut().poll_next(cx)
    }
}

pub struct Response {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: Body,
}

impl Default for Response {
    fn default() -> Self {
        Self {
            status: 200,
            headers: HeaderMap::new(),
            body: Body::stream(futures_util::stream::empty()),
        }
    }
}

impl Response {
    pub fn json(value: impl Serialize) -> Result<Self, BoxError> {
        let mut response = Self::bytes(serde_json::to_vec(&value)?);
        response.headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        Ok(response)
    }

    pub fn text(value: impl Into<String>) -> Self {
        let mut response = Self::bytes(value.into());
        response.headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("text/plain; charset=utf-8"),
        );
        response
    }

    pub fn bytes(value: impl Into<Bytes>) -> Self {
        Self {
            body: Body::bytes(value),
            ..Self::default()
        }
    }

    pub fn stream(stream: impl Stream<Item = Result<Bytes, BoxError>> + Send + 'static) -> Self {
        Self {
            body: Body::stream(stream),
            ..Self::default()
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Event {
    pub id: String,
    #[serde(rename = "type")]
    pub event_type: String,
    pub source: String,
    pub subject: String,
    pub time: String,
    pub data: serde_json::Value,
    pub attempt: u64,
}

type Transport = WebSocketStream<TokioIo<Upgraded>>;
type Sink = SplitSink<Transport, Message>;
const RECEIVE_QUEUE: usize = 16;
const SEND_TIMEOUT: Duration = Duration::from_secs(30);

struct WebSocketWriter {
    sink: Sink,
    closing: bool,
}

impl WebSocketWriter {
    async fn send(&mut self, message: Message) -> Result<(), WebSocketError> {
        let closing = message.is_close();
        self.sink.send(message).await?;
        self.closing |= closing;
        Ok(())
    }
}

async fn close_websocket(writer: &Mutex<WebSocketWriter>, code: CloseCode, reason: &str) {
    let closing = async {
        let mut writer = writer.lock().await;
        if !writer.closing {
            writer
                .send(Message::Close(Some(CloseFrame {
                    code,
                    reason: reason.into(),
                })))
                .await?;
        }
        Ok::<(), WebSocketError>(())
    };
    match tokio::time::timeout(Duration::from_secs(1), closing).await {
        Ok(Ok(())) | Ok(Err(WebSocketError::ConnectionClosed | WebSocketError::AlreadyClosed)) => {}
        Ok(Err(error)) => eprintln!("WebSocket close error: {error}"),
        Err(_) => eprintln!("WebSocket close timed out"),
    }
}

pub(crate) struct WebSocketDriver {
    writer: Arc<Mutex<WebSocketWriter>>,
    reader: JoinHandle<()>,
    cancelled: CancellationToken,
}

impl WebSocketDriver {
    pub(crate) async fn finish(self, code: CloseCode, reason: &str) {
        close_websocket(&self.writer, code, reason).await;
        self.cancelled.cancel();
        self.reader.abort();
        if let Err(error) = self.reader.await {
            if !error.is_cancelled() {
                eprintln!("WebSocket reader error: {error}");
            }
        }
    }
}

pub struct WebSocketSession {
    writer: Arc<Mutex<WebSocketWriter>>,
    messages: tokio::sync::mpsc::Receiver<Result<Message, BoxError>>,
    cancelled: CancellationToken,
}

impl WebSocketSession {
    /// Starts the adapter-owned socket reader so peer disconnects cancel idle hooks.
    pub(crate) fn start(
        socket: Transport,
        cancelled: CancellationToken,
    ) -> (Self, WebSocketDriver) {
        let (sink, mut stream) = socket.split();
        let writer = Arc::new(Mutex::new(WebSocketWriter {
            sink,
            closing: false,
        }));
        let (sender, messages) = tokio::sync::mpsc::channel(RECEIVE_QUEUE);
        let reader_writer = writer.clone();
        let reader_token = cancelled.clone();
        let reader = tokio::spawn(async move {
            let _stop = reader_token.clone().drop_guard();
            loop {
                let next = tokio::select! {
                    _ = reader_token.cancelled() => return,
                    next = stream.next() => next,
                };
                let Some(next) = next else { return };
                let message = match next {
                    Ok(message) => {
                        // tungstenite queues pong/close replies while reading; only
                        // those need a flush. Data frames must not wait on a writer
                        // that a slow peer has stalled, or the session is torn down.
                        // A queued pong also goes out with the writer's next frame,
                        // so a busy or slow writer only defers it.
                        if message.is_ping() {
                            if let Ok(mut writer) = reader_writer.try_lock() {
                                let flushed = tokio::select! {
                                    _ = reader_token.cancelled() => return,
                                    flushed = tokio::time::timeout(Duration::from_secs(1), writer.sink.flush()) => flushed,
                                };
                                if let Ok(Err(error)) = flushed {
                                    eprintln!("WebSocket flush error: {error}");
                                    let _ = sender.try_send(Err(error.into()));
                                    return;
                                }
                            }
                        } else if message.is_close() {
                            tokio::select! {
                                _ = reader_token.cancelled() => return,
                                flushed = async {
                                    tokio::time::timeout(Duration::from_secs(1), async {
                                        let mut writer = reader_writer.lock().await;
                                        writer.closing |= message.is_close();
                                        writer.sink.flush().await
                                    }).await??;
                                    Ok::<(), BoxError>(())
                                } => {
                                    if let Err(error) = flushed {
                                        eprintln!("WebSocket flush error: {error}");
                                        let _ = sender.try_send(Err(error));
                                        return;
                                    }
                                }
                            }
                        }
                        message
                    }
                    Err(error) => {
                        eprintln!("WebSocket receive error: {error}");
                        let code = match &error {
                            WebSocketError::Utf8(_) => Some(CloseCode::Invalid),
                            WebSocketError::Capacity(_) => Some(CloseCode::Size),
                            WebSocketError::Protocol(_) => Some(CloseCode::Protocol),
                            _ => None,
                        };
                        if let Some(code) = code {
                            close_websocket(&reader_writer, code, "").await;
                        }
                        let _ = sender.try_send(Err(error.into()));
                        return;
                    }
                };
                if message.is_close() {
                    let _ = sender.try_send(Ok(message));
                    return;
                }
                // tungstenite answers pings itself; keepalives must not fill the
                // handler's queue and overload a session that never calls recv().
                if message.is_ping() || message.is_pong() {
                    continue;
                }
                // A full queue overloads the session instead of blocking disconnect detection.
                match sender.try_send(Ok(message)) {
                    Ok(()) => {}
                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                        eprintln!("WebSocket receive queue overloaded");
                        close_websocket(&reader_writer, CloseCode::Again, "overloaded").await;
                        return;
                    }
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => return,
                }
            }
        });
        (
            Self {
                writer: writer.clone(),
                messages,
                cancelled: cancelled.clone(),
            },
            WebSocketDriver {
                writer,
                reader,
                cancelled,
            },
        )
    }

    pub async fn recv(&mut self) -> Result<Option<Message>, BoxError> {
        tokio::select! {
            biased;
            message = self.messages.recv() => message.transpose(),
            _ = self.cancelled.cancelled() => Ok(None),
        }
    }

    pub async fn send(&mut self, message: Message) -> Result<(), BoxError> {
        // Bound each send so a peer that stops reading cannot pin the hook.
        let send = tokio::time::timeout(SEND_TIMEOUT, async {
            self.writer.lock().await.send(message).await
        });
        tokio::select! {
            biased;
            _ = self.cancelled.cancelled() => Err(WebSocketError::ConnectionClosed.into()),
            result = send => match result {
                Ok(result) => Ok(result?),
                Err(_) => Err("WebSocket send timed out".into()),
            },
        }
    }
}

type HttpHook = Arc<dyn Fn(Request, Context) -> BoxFuture<'static, HandlerResult> + Send + Sync>;
type EventHook =
    Arc<dyn Fn(Event, ProtocolContext) -> BoxFuture<'static, HandlerResult> + Send + Sync>;
type WebSocketHook = Arc<
    dyn Fn(WebSocketSession, ProtocolContext) -> BoxFuture<'static, Result<(), BoxError>>
        + Send
        + Sync,
>;

/// Required HTTP hook and optional, independently registered protocol hooks.
pub struct Hooks {
    pub(crate) handle: HttpHook,
    pub(crate) event: Option<EventHook>,
    pub(crate) websocket: Option<WebSocketHook>,
}

impl Hooks {
    pub fn new<F, Fut>(handle: F) -> Self
    where
        F: Fn(Request, Context) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HandlerResult> + Send + 'static,
    {
        Self {
            handle: Arc::new(move |r, c| Box::pin(handle(r, c))),
            event: None,
            websocket: None,
        }
    }

    pub fn event<F, Fut>(mut self, handle: F) -> Self
    where
        F: Fn(Event, ProtocolContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HandlerResult> + Send + 'static,
    {
        self.event = Some(Arc::new(move |e, c| Box::pin(handle(e, c))));
        self
    }

    pub fn websocket<F, Fut>(mut self, handle: F) -> Self
    where
        F: Fn(WebSocketSession, ProtocolContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), BoxError>> + Send + 'static,
    {
        self.websocket = Some(Arc::new(move |s, c| Box::pin(handle(s, c))));
        self
    }
}
