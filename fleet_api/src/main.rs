use axum::{
    routing::{get, post},
    Router, Json, http::StatusCode,
    extract::State,
    response::IntoResponse,
};
use jsonwebtoken::{decode, DecodingKey, Validation, Algorithm};
use serde::{Deserialize, Serialize};
use shared_types::NodeTelemetry;
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::Arc,
};
use tower_http::cors::CorsLayer;
use tokio::sync::RwLock;
use axum::{
    extract::Request,
    middleware::{self, Next},
    response::Response,
};

// App State holding the live fleet telemetry
type AppState = Arc<RwLock<HashMap<String, NodeTelemetry>>>;

#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    sub: String, // The hostname (e.g. "sbl1")
    exp: usize,
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    repository: &'static str,
    version: &'static str,
}

#[tokio::main]
async fn main() {
    let state = Arc::new(RwLock::new(HashMap::new()));

    // Public routes (No Auth)
    let public_app = Router::new()
        .route("/health", get(health_handler))
        .route("/telemetry", get(get_telemetry))
        .with_state(state.clone());

    // Protected routes (Require JWT Auth)
    let protected_app = Router::new()
        .route("/telemetry", post(post_telemetry))
        .route_layer(middleware::from_fn_with_state(state.clone(), auth_middleware))
        .with_state(state.clone());

    // Merge them into one application and add permissive CORS for Next.js browser requests
    let app = public_app.merge(protected_app).layer(CorsLayer::permissive());

    let addr = SocketAddr::from(([127, 0, 0, 1], 4000));
    println!("Fleet Core API running on http://{}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn health_handler() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "online",
        repository: "antimony-labs/core",
        version: env!("CARGO_PKG_VERSION"),
    })
}

async fn auth_middleware(
    State(_state): State<AppState>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    // 1. Extract the Authorization header
    let auth_header = req.headers().get("Authorization").and_then(|h| h.to_str().ok());
    let token = match auth_header {
        Some(header) if header.starts_with("Bearer ") => &header[7..],
        _ => return Err(StatusCode::UNAUTHORIZED),
    };

    // 2. Decode the JWT using the pre-shared Ed25519 Public Key
    // Note: In production this should be loaded from env/vault. Hardcoded mock for TDD.
    let public_key_pem = include_bytes!("../public_key.pem");
    let decoding_key = DecodingKey::from_ed_pem(public_key_pem).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    
    let mut validation = Validation::new(Algorithm::EdDSA);
    validation.set_required_spec_claims(&["exp", "sub"]);

    let _token_data = decode::<Claims>(token, &decoding_key, &validation).map_err(|_| StatusCode::UNAUTHORIZED)?;

    // You could inject the Claims into the request extensions here if needed
    // req.extensions_mut().insert(token_data.claims);

    Ok(next.run(req).await)
}

async fn post_telemetry(
    State(state): State<AppState>,
    Json(payload): Json<NodeTelemetry>,
) -> impl IntoResponse {
    let mut map = state.write().await;
    map.insert(payload.hostname.clone(), payload);
    StatusCode::OK
}

async fn get_telemetry(
    State(state): State<AppState>,
) -> Json<Vec<NodeTelemetry>> {
    let map = state.read().await;
    let nodes: Vec<NodeTelemetry> = map.values().cloned().collect();
    Json(nodes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{self, Request, StatusCode},
    };
    use tower::ServiceExt; // for `oneshot` and `ready`
    use serde_json::json;

    // Helper function to build the Router for testing
    fn build_test_app() -> Router {
        let state = Arc::new(RwLock::new(HashMap::new()));
        let public_app = Router::new()
            .route("/telemetry", get(get_telemetry))
            .with_state(state.clone());

        let protected_app = Router::new()
            .route("/telemetry", post(post_telemetry))
            .route_layer(middleware::from_fn_with_state(state.clone(), auth_middleware))
            .with_state(state);

        public_app.merge(protected_app)
    }

    #[tokio::test]
    async fn test_unauthorized_post_rejected() {
        let app = build_test_app();

        let req = Request::builder()
            .method(http::Method::POST)
            .uri("/telemetry")
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(json!({
                "hostname": "hacker_node",
                "cpu_usage": 99.0,
                "ram_used_mb": 1024,
                "ram_total_mb": 2048,
                "tailscale_ip": "100.0.0.1",
                "timestamp_sec": 123456789
            }).to_string()))
            .unwrap();

        let response = app.oneshot(req).await.unwrap();
        // Should be rejected by the JWT middleware because it lacks the Authorization header
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    
    #[tokio::test]
    async fn test_authorized_post_accepted() {
        use jsonwebtoken::{encode, EncodingKey, Header};
        use std::time::{SystemTime, UNIX_EPOCH};

        let state = Arc::new(RwLock::new(HashMap::new()));
        let public_app = Router::new()
            .route("/telemetry", get(get_telemetry))
            .with_state(state.clone());

        let protected_app = Router::new()
            .route("/telemetry", post(post_telemetry))
            .route_layer(middleware::from_fn_with_state(state.clone(), auth_middleware))
            .with_state(state.clone());

        let app = public_app.merge(protected_app);

        // 1. Generate a valid JWT
        let private_key_pem = include_bytes!("../private_key.pem");
        let encoding_key = EncodingKey::from_ed_pem(private_key_pem).unwrap();
        let exp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() + 60;
        let claims = Claims {
            sub: "test_node_01".to_string(),
            exp: exp as usize,
        };
        let token = encode(&Header::new(Algorithm::EdDSA), &claims, &encoding_key).unwrap();

        // 2. Build the authorized POST request
        let req = Request::builder()
            .method(http::Method::POST)
            .uri("/telemetry")
            .header(http::header::CONTENT_TYPE, "application/json")
            .header(http::header::AUTHORIZATION, format!("Bearer {}", token))
            .body(Body::from(json!({
                "hostname": "test_node_01",
                "cpu_usage": 45.5,
                "ram_used_mb": 2048,
                "ram_total_mb": 16384,
                "tailscale_ip": "100.0.0.99",
                "timestamp_sec": 123456789
            }).to_string()))
            .unwrap();

        // 3. Execute request
        let response = app.oneshot(req).await.unwrap();
        
        // 4. Assert 200 OK
        assert_eq!(response.status(), StatusCode::OK);

        // 5. Assert the state actually updated in memory
        let map = state.read().await;
        assert_eq!(map.len(), 1);
        let stored_node = map.get("test_node_01").unwrap();
        assert_eq!(stored_node.cpu_usage, 45.5);
    }
}
