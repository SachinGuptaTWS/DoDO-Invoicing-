use harness::webhook_sink::WebhookSink;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    let addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:9191".into());
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(%addr, "webhook sink listening");
    axum::serve(listener, WebhookSink::default().router()).await
}
