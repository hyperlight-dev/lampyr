pub fn routes() -> axum::Router {
    axum::Router::new().fallback(|uri: axum::http::Uri| async move {
        axum::Json(serde_json::json!({
            "message": "hello from Rust",
            "path": uri.path(),
        }))
    })
}
