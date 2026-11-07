use crate::types::{
    BoxError, Context, Event, HandlerResult, Hooks, Message, ProtocolContext, Request, Response,
    WebSocketSession,
};
use futures_util::stream;
use serde_json::json;

fn log(message: &str, path: &str, event: Option<&Event>) {
    let timestamp = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .expect("current time is RFC3339");
    println!(
        "{}",
        json!({
            "timestamp": timestamp, "file": "handler.rs", "kind": "handler",
            "service": "hello-rust", "message": message, "path": path, "event": event,
        })
    );
}

pub fn hooks() -> Hooks {
    Hooks::new(handle)
        .event(handle_event)
        .websocket(handle_websocket)
}

pub async fn handle(request: Request, _context: Context) -> HandlerResult {
    log("request received", &request.path, None);
    if request.path == "/stream" {
        let chunks = (1..=3).map(|chunk| Ok(format!("{{\"chunk\":{chunk}}}\n").into()));
        let mut response = Response::stream(stream::iter(chunks));
        response
            .headers
            .insert("content-type", "application/x-ndjson".parse()?);
        return Ok(response);
    }
    Response::json(json!({
        "message": "hello from Rust on Hyperlight edge",
        "path": request.path,
    }))
}

pub async fn handle_event(event: Event, context: ProtocolContext) -> HandlerResult {
    let request = context.request;
    log("event received", &request.path, Some(&event));
    let mut response = Response::json(json!({"event": event}))?;
    response.status = match request.path.as_str() {
        "/events/fail" => 500,
        "/events/knative" => 204,
        _ => 200,
    };
    Ok(response)
}

pub async fn handle_websocket(
    mut session: WebSocketSession,
    _context: ProtocolContext,
) -> Result<(), BoxError> {
    while let Some(message) = session.recv().await? {
        match message {
            Message::Text(_) | Message::Binary(_) => session.send(message).await?,
            Message::Close(_) => break,
            _ => {}
        }
    }
    Ok(())
}
