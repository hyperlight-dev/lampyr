// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use crate::types::{
    Body, BoxError, Bytes, Context, HandlerError, HandlerResult, Hooks, Request, Response,
};
use futures_util::{stream, StreamExt};
use serde::{Serialize, Serializer};
use serde_json::json;
use std::{io, time::Duration};

static CLOSED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

struct CloseRecorder {
    path: String,
}

impl futures_util::Stream for CloseRecorder {
    type Item = Result<Bytes, BoxError>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        std::task::Poll::Ready(Some(Err(io::Error::other(
            "bodyless response source was read",
        )
        .into())))
    }
}

impl Drop for CloseRecorder {
    fn drop(&mut self) {
        CLOSED.lock().unwrap().push(std::mem::take(&mut self.path));
    }
}

pub fn hooks() -> Hooks {
    Hooks::new(handle)
}

struct SerializationFailure;

impl Serialize for SerializationFailure {
    fn serialize<S: Serializer>(&self, _serializer: S) -> Result<S::Ok, S::Error> {
        Err(serde::ser::Error::custom("private details"))
    }
}

struct Cleanup;

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::write("cancelled", "closed");
    }
}

pub async fn handle(mut request: Request, context: Context) -> HandlerResult {
    match request.path.as_str() {
        "/http-context" => {
            let Context { cancelled: _ } = context;
            Response::json(json!({"requestInContext": false}))
        }
        "/upload-stream" => Ok(Response {
            body: request.body,
            ..Response::default()
        }),
        "/own-nosniff" => {
            let mut response = Response::text("own");
            response
                .headers
                .insert("x-content-type-options", "handler".parse()?);
            Ok(response)
        }
        "/repeated-headers" => {
            let values = request
                .headers
                .get_all("x-repeat")
                .iter()
                .map(|value| value.to_str())
                .collect::<Result<Vec<_>, _>>()?;
            let mut response = Response::json(json!({"values": values}))?;
            response.headers.append("set-cookie", "a=1".parse()?);
            response.headers.append("set-cookie", "b=2".parse()?);
            Ok(response)
        }
        "/json-body" => {
            let value: serde_json::Value = request.body.json().await?;
            Response::json(value)
        }
        "/value" => {
            let mut response = Response::json(json!({"ok": true}))?;
            response.status = 201;
            response.headers.insert("x-contract", "value".parse()?);
            Ok(response)
        }
        "/bytes" => Ok(Response::bytes(Bytes::from_static(b"raw\0bytes"))),
        "/text" => {
            let mut response = Response::bytes("plain text");
            response
                .headers
                .insert("content-type", "text/plain".parse()?);
            Ok(response)
        }
        "/array" => Response::json([1, 2, 3]),
        "/stream" => Ok(Response::stream(stream::iter(
            ["one\n", "two\n", "three\n"].map(|value| Ok(Bytes::from_static(value.as_bytes()))),
        ))),
        "/typed-error" => Err(HandlerError::new(422, "<bad>\r\nX-Injected: yes").into()),
        "/error" => Err(io::Error::other("private details").into()),
        "/serialize-error" => Response::json(SerializationFailure),
        "/empty" => Ok(Response {
            status: 204,
            ..Response::bytes("must not appear")
        }),
        "/unicode-header" => {
            let mut response = Response::bytes("bad");
            response
                .headers
                .insert("x-locale", "snowman \u{2603}".parse()?);
            Ok(response)
        }
        "/headers" | "/transfer-encoding" | "/connection" => {
            let mut response = Response::bytes("bad");
            let (key, value) = match request.path.as_str() {
                "/headers" => ("content-length", "999"),
                "/transfer-encoding" => ("transfer-encoding", "chunked"),
                _ => ("connection", "close"),
            };
            response
                .headers
                .insert(http::header::HeaderName::from_static(key), value.parse()?);
            Ok(response)
        }
        "/invalid-status" => Ok(Response {
            status: 999,
            ..Response::default()
        }),
        "/closed-bodies" => Response::json(CLOSED.lock().unwrap().clone()),
        "/close-head" | "/close-204" | "/close-304" => Ok(Response {
            status: match request.path.as_str() {
                "/close-204" => 204,
                "/close-304" => 304,
                _ => 200,
            },
            body: Body::stream(CloseRecorder {
                path: request.path.clone(),
            }),
            ..Response::default()
        }),
        "/not-modified" => Ok(Response {
            status: 304,
            ..Response::bytes("must not appear")
        }),
        "/broken-stream" => Ok(Response::stream(stream::iter([
            Ok(Bytes::from_static(b"first")),
            Err::<Bytes, BoxError>(io::Error::other("stream failed").into()),
        ]))),
        "/backpressure" => Ok(Response::stream(stream::iter(1..=128).map(|index| {
            std::fs::write("produced", index.to_string())?;
            Ok(Bytes::from(vec![b'x'; 1024 * 1024]))
        }))),
        "/cancel" => {
            let cleanup = Cleanup;
            let chunks = stream::unfold(
                (context.cancelled, cleanup),
                |(token, cleanup)| async move {
                    tokio::select! {
                        _ = token.cancelled() => None,
                        _ = tokio::time::sleep(Duration::from_millis(5)) => {
                            Some((Ok(Bytes::from(vec![b'x'; 65536])), (token, cleanup)))
                        }
                    }
                },
            );
            Ok(Response::stream(chunks))
        }
        _ => {
            let body = request.body.text().await?;
            Response::json(json!({
                "method": request.method,
                "path": request.path,
                "body": body,
                "header": request.headers.get("x-test").and_then(|value| value.to_str().ok()),
            }))
        }
    }
}
