mod application;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let port: u16 = std::env::var("PORT").unwrap_or_else(|_| "8080".into()).parse()?;
    let host = std::env::var("LAMPYR_LISTEN_HOST").unwrap_or_else(|_| "0.0.0.0".into());
    let listener = tokio::net::TcpListener::bind((host.as_str(), port)).await?;
    axum::serve(listener, application::routes())
        .with_graceful_shutdown(async {
            tokio::signal::ctrl_c().await.expect("install shutdown signal handler");
        })
        .await?;
    Ok(())
}
