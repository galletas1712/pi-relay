#![forbid(unsafe_code)]

mod config;

use std::sync::Arc;

use agent_store::PostgresAgentStore;
use anyhow::Result;
use config::Config;

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::from_env_and_args()?;
    let store = Arc::new(PostgresAgentStore::connect(&config.database_url).await?);
    // pi-agentd is the sole migration owner. This read verifies that its
    // migration completed and that pi-web's role can SELECT the required view.
    store.load_session_git_config("").await?;
    let app = pi_web::router_with_executables(
        store.clone(),
        config.web_root,
        config.allowed_hosts,
        config.git_executables,
    )?;
    let listener = tokio::net::TcpListener::bind(config.bind).await?;
    eprintln!("pi-web listening on http://{}", listener.local_addr()?);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    store.close().await;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl-C handler");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}
