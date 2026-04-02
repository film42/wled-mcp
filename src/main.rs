mod discovery;
mod models;
mod server;
mod wled;

use rmcp::ServiceExt;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "wled_mcp=info".parse().unwrap()),
        )
        .with_writer(std::io::stderr)
        .init();

    tracing::info!("starting WLED MCP server");

    let server = server::WledServer::new();
    let service = server
        .serve((tokio::io::stdin(), tokio::io::stdout()))
        .await
        .inspect_err(|e| tracing::error!("serving error: {:?}", e))?;

    service.waiting().await?;
    Ok(())
}
