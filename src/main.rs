use axum::{
    routing::get,
    Router,
    Json,
};
use serde::Serialize;
use std::net::SocketAddr;

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    repository: &'static str,
    version: &'static str,
}

#[tokio::main]
async fn main() {
    let app = Router::new()
        .route("/health", get(|| async { 
            Json(HealthResponse {
                status: "online",
                repository: "antimony-labs/core",
                version: env!("CARGO_PKG_VERSION"),
            })
        }));

    // Bind to localhost because Cloudflare Tunnels (cloudflared) will proxy it out securely
    let addr = SocketAddr::from(([127, 0, 0, 1], 4000));
    println!("API Core running on http://{}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
