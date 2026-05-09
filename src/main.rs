mod game;
mod proto;
mod ws;

use axum::{Router, routing::get};
use tower_http::services::ServeDir;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,agent_wars=debug")),
        )
        .init();

    let state = ws::AppState::new();

    let app = Router::new()
        .route("/ws", get(ws::ws_handler))
        .fallback_service(ServeDir::new("web"))
        .with_state(state);

    let addr = "127.0.0.1:8080";
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    tracing::info!("listening on http://{addr}");
    axum::serve(listener, app).await.unwrap();
}
