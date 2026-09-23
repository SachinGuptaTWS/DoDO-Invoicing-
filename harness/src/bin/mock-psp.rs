use harness::mock_psp::{MockPsp, MockPspConfig};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    let addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:9090".into());
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(%addr, "mock PSP listening");
    axum::serve(listener, MockPsp::new(MockPspConfig::default()).router()).await
}
