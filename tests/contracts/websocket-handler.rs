// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use crate::types::{
    BoxError, Context, HandlerResult, Hooks, Message, ProtocolContext, Request, Response,
    WebSocketSession,
};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio_tungstenite::tungstenite::{
    protocol::{frame::coding::CloseCode, CloseFrame},
    Error as WebSocketError,
};

static DISCONNECTED: AtomicBool = AtomicBool::new(false);
static ACTIVE: AtomicBool = AtomicBool::new(false);
static SENT: AtomicUsize = AtomicUsize::new(0);
static CANCELLED_SEND_FAILED: AtomicBool = AtomicBool::new(false);

pub fn hooks() -> Hooks {
    Hooks::new(handle).websocket(handle_websocket)
}

async fn handle(_: Request, _: Context) -> HandlerResult {
    Response::json(serde_json::json!({
        "disconnected": DISCONNECTED.load(Ordering::SeqCst),
        "active": ACTIVE.load(Ordering::SeqCst),
        "sent": SENT.load(Ordering::SeqCst),
        "cancelledSendFailed": CANCELLED_SEND_FAILED.load(Ordering::SeqCst),
    }))
}

async fn handle_websocket(
    mut session: WebSocketSession,
    context: ProtocolContext,
) -> Result<(), BoxError> {
    match context.request.path.as_str() {
        "/complete" => return Ok(()),
        "/fail" => return Err("WebSocket handler failed".into()),
        "/panic" => panic!("WebSocket handler panicked"),
        "/cancel-send" => {
            context.cancelled.cancel();
            let result = session.send(Message::Text("must not send".into())).await;
            CANCELLED_SEND_FAILED.store(
                result.err().is_some_and(|error| {
                    matches!(
                        error.downcast_ref::<WebSocketError>(),
                        Some(WebSocketError::ConnectionClosed)
                    )
                }),
                Ordering::SeqCst,
            );
            return Ok(());
        }
        "/close" => {
            return session
                .send(Message::Close(Some(CloseFrame {
                    code: CloseCode::from(4000),
                    reason: "finished".into(),
                })))
                .await;
        }
        _ => {}
    }
    // The adapter drops the hook future on cancellation, so observe it via drop.
    struct Observed;
    impl Drop for Observed {
        fn drop(&mut self) {
            ACTIVE.store(false, Ordering::SeqCst);
            DISCONNECTED.store(true, Ordering::SeqCst);
        }
    }
    let _observed = Observed;
    ACTIVE.store(true, Ordering::SeqCst);
    if context.request.path == "/send" {
        let message = Message::Binary(vec![0; 1024 * 1024].into());
        loop {
            session.send(message.clone()).await?;
            SENT.fetch_add(1, Ordering::SeqCst);
        }
    }
    context.cancelled.cancelled().await;
    Ok(())
}
