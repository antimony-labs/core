use axum::{
    Json, Router,
    extract::{Query, Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::Response,
    routing::{get, post},
};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode};
use serde::{Deserialize, Serialize};
use shared_types::NodeTelemetry;
use sqlx::SqlitePool;
use sqlx::sqlite::SqlitePoolOptions;
use std::{
    collections::BTreeMap,
    net::SocketAddr,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

// App State holding the SQLite Connection Pool
type AppState = SqlitePool;
const DEFAULT_HISTORY_HOURS: u64 = 24;
const MAX_HISTORY_HOURS: u64 = 24 * 7;
const RETENTION_DAYS: i64 = 14;
const CLEANUP_INTERVAL_SECS: u64 = 60 * 60;

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

#[derive(Debug, Deserialize)]
struct TelemetryQuery {
    hours: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
struct TelemetryHistoryResponse {
    window_hours: u64,
    generated_at_sec: i64,
    nodes: BTreeMap<String, Vec<NodeTelemetry>>,
}

#[tokio::main]
async fn main() {
    // Initialize DB Pool
    let db_url = "sqlite://telemetry.db";
    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect(db_url)
        .await
        .expect("Failed to bind sqlite memory pool");

    // Run migrations
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("Unable to migrate database schema");

    tokio::spawn(run_retention_job(pool.clone()));

    let state = pool;

    // Public routes (No Auth)
    let public_app = Router::new()
        .route("/health", get(health_handler))
        .route("/telemetry", get(get_telemetry))
        .route(
            "/telemetry",
            axum::routing::options(|| async { axum::http::StatusCode::OK }),
        )
        .with_state(state.clone());

    // Protected routes (Require JWT Auth)
    let protected_app = Router::new()
        .route("/telemetry", post(post_telemetry))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state.clone());

    // Merge them into one application and add permissive CORS for Next.js browser requests
    let app = public_app
        .merge(protected_app)
        .layer(middleware::from_fn(cors_middleware));

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
    if req.method() == axum::http::Method::OPTIONS {
        return Ok(next.run(req).await);
    }

    // 1. Extract the Authorization header
    let auth_header = req
        .headers()
        .get("Authorization")
        .and_then(|h| h.to_str().ok());
    let token = match auth_header {
        Some(header) if header.starts_with("Bearer ") => &header[7..],
        _ => return Err(StatusCode::UNAUTHORIZED),
    };

    // 2. Decode the JWT using the pre-shared Ed25519 Public Key
    // Note: In production this should be loaded from env/vault. Hardcoded mock for TDD.
    let public_key_pem = include_bytes!("../public_key.pem");
    let decoding_key =
        DecodingKey::from_ed_pem(public_key_pem).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let mut validation = Validation::new(Algorithm::EdDSA);
    validation.set_required_spec_claims(&["exp", "sub"]);

    let _token_data = decode::<Claims>(token, &decoding_key, &validation)
        .map_err(|_| StatusCode::UNAUTHORIZED)?;

    // You could inject the Claims into the request extensions here if needed
    // req.extensions_mut().insert(token_data.claims);

    Ok(next.run(req).await)
}

async fn post_telemetry(
    State(pool): State<AppState>,
    Json(payload): Json<NodeTelemetry>,
) -> Result<StatusCode, StatusCode> {
    let id = Uuid::new_v4().to_string();

    sqlx::query(
        r#"
        INSERT INTO telemetry 
            (id, hostname, cpu_usage, ram_used_mb, ram_total_mb, load_avg_1m, load_avg_5m, load_avg_15m, uptime_secs, disk_used_gb, disk_total_gb, tailscale_ip, timestamp_sec) 
        VALUES 
            (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#
    )
    .bind(id)
    .bind(&payload.hostname)
    .bind(payload.cpu_usage)
    .bind(payload.ram_used_mb as i64)
    .bind(payload.ram_total_mb as i64)
    .bind(payload.load_avg_1m)
    .bind(payload.load_avg_5m)
    .bind(payload.load_avg_15m)
    .bind(payload.uptime_secs as i64)
    .bind(payload.disk_used_gb)
    .bind(payload.disk_total_gb)
    .bind(&payload.tailscale_ip)
    .bind(payload.timestamp_sec)
    .execute(&pool)
    .await
    .map_err(|e| {
        eprintln!("Failed to insert telemetry: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok(StatusCode::OK)
}

async fn get_telemetry(
    State(pool): State<AppState>,
    Query(query): Query<TelemetryQuery>,
) -> Result<Json<TelemetryHistoryResponse>, StatusCode> {
    let hours = query
        .hours
        .unwrap_or(DEFAULT_HISTORY_HOURS)
        .clamp(1, MAX_HISTORY_HOURS);
    let now = current_timestamp_sec();
    let min_timestamp = now - (hours as i64 * 60 * 60);

    let records = sqlx::query_as::<_, NodeTelemetry>(
        r#"
        SELECT 
            hostname, cpu_usage, ram_used_mb, ram_total_mb,
            load_avg_1m, load_avg_5m, load_avg_15m,
            uptime_secs, disk_used_gb, disk_total_gb,
            tailscale_ip, timestamp_sec
        FROM telemetry 
        WHERE timestamp_sec >= ?
        ORDER BY hostname ASC, timestamp_sec ASC
        "#,
    )
    .bind(min_timestamp)
    .fetch_all(&pool)
    .await
    .map_err(|e| {
        eprintln!("Failed to fetch telemetry history: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let mut nodes: BTreeMap<String, Vec<NodeTelemetry>> = BTreeMap::new();
    for record in records {
        nodes
            .entry(record.hostname.clone())
            .or_default()
            .push(record);
    }

    Ok(Json(TelemetryHistoryResponse {
        window_hours: hours,
        generated_at_sec: now,
        nodes,
    }))
}

async fn cors_middleware(req: Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    res.headers_mut()
        .insert("Access-Control-Allow-Origin", "*".parse().unwrap());
    res.headers_mut().insert(
        "Access-Control-Allow-Methods",
        "GET, POST, OPTIONS".parse().unwrap(),
    );
    res.headers_mut().insert(
        "Access-Control-Allow-Headers",
        "Content-Type, Authorization".parse().unwrap(),
    );
    res
}

fn current_timestamp_sec() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before unix epoch")
        .as_secs() as i64
}

async fn run_retention_job(pool: SqlitePool) {
    let mut interval = tokio::time::interval(Duration::from_secs(CLEANUP_INTERVAL_SECS));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        interval.tick().await;

        match cleanup_old_telemetry(&pool).await {
            Ok(rows_deleted) if rows_deleted > 0 => {
                println!(
                    "Pruned {} telemetry rows older than {} days",
                    rows_deleted, RETENTION_DAYS
                );
            }
            Ok(_) => {}
            Err(err) => eprintln!("Failed to prune telemetry rows: {}", err),
        }
    }
}

async fn cleanup_old_telemetry(pool: &SqlitePool) -> Result<u64, sqlx::Error> {
    let cutoff = current_timestamp_sec() - (RETENTION_DAYS * 24 * 60 * 60);
    let result = sqlx::query("DELETE FROM telemetry WHERE timestamp_sec < ?")
        .bind(cutoff)
        .execute(pool)
        .await?;

    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::{self, Request, StatusCode},
    };
    use serde_json::json;
    use sqlx::Row;
    use tower::ServiceExt; // for `oneshot` and `ready`

    // Helper function to build the Router for testing
    async fn build_test_app() -> (Router, SqlitePool) {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS telemetry (
                id TEXT PRIMARY KEY NOT NULL,
                hostname TEXT NOT NULL,
                cpu_usage REAL NOT NULL,
                ram_used_mb INTEGER NOT NULL,
                ram_total_mb INTEGER NOT NULL,
                load_avg_1m REAL NOT NULL,
                load_avg_5m REAL NOT NULL,
                load_avg_15m REAL NOT NULL,
                uptime_secs INTEGER NOT NULL,
                disk_used_gb REAL NOT NULL,
                disk_total_gb REAL NOT NULL,
                tailscale_ip TEXT NOT NULL,
                timestamp_sec INTEGER NOT NULL
            );
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();

        let public_app = Router::new()
            .route("/telemetry", get(get_telemetry))
            .route(
                "/telemetry",
                axum::routing::options(|| async { axum::http::StatusCode::OK }),
            )
            .with_state(pool.clone());

        let protected_app = Router::new()
            .route("/telemetry", post(post_telemetry))
            .route_layer(middleware::from_fn_with_state(
                pool.clone(),
                auth_middleware,
            ))
            .with_state(pool.clone());

        let app = public_app
            .merge(protected_app)
            .layer(middleware::from_fn(cors_middleware));
        (app, pool)
    }

    #[tokio::test]
    async fn test_unauthorized_post_rejected() {
        let (app, _) = build_test_app().await;

        let req = Request::builder()
            .method(http::Method::POST)
            .uri("/telemetry")
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({
                    "hostname": "hacker_node",
                    "cpu_usage": 99.0,
                    "ram_used_mb": 1024,
                    "ram_total_mb": 2048,
                    "load_avg_1m": 4.0,
                    "load_avg_5m": 3.5,
                    "load_avg_15m": 3.0,
                    "uptime_secs": 86400,
                    "disk_used_gb": 50.0,
                    "disk_total_gb": 100.0,
                    "tailscale_ip": "100.0.0.1",
                    "timestamp_sec": 123456789
                })
                .to_string(),
            ))
            .unwrap();

        let response = app.oneshot(req).await.unwrap();
        // Should be rejected by the JWT middleware because it lacks the Authorization header
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_authorized_post_accepted() {
        use jsonwebtoken::{EncodingKey, Header, encode};

        let (app, pool) = build_test_app().await;

        // 1. Generate a valid JWT
        let private_key_pem = include_bytes!("../private_key.pem");
        let encoding_key = EncodingKey::from_ed_pem(private_key_pem).unwrap();
        let exp = current_timestamp_sec() as u64 + 60;
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
            .body(Body::from(
                json!({
                    "hostname": "test_node_01",
                    "cpu_usage": 45.5,
                    "ram_used_mb": 2048,
                    "ram_total_mb": 16384,
                    "load_avg_1m": 1.5,
                    "load_avg_5m": 1.2,
                    "load_avg_15m": 1.0,
                    "uptime_secs": 3600,
                    "disk_used_gb": 25.0,
                    "disk_total_gb": 500.0,
                    "tailscale_ip": "100.0.0.99",
                    "timestamp_sec": 123456789
                })
                .to_string(),
            ))
            .unwrap();

        // 3. Execute request
        let response = app.oneshot(req).await.unwrap();

        // 4. Assert 200 OK
        assert_eq!(response.status(), StatusCode::OK);

        // 5. Assert the state actually updated the SQL DB
        let record = sqlx::query("SELECT cpu_usage FROM telemetry WHERE hostname = 'test_node_01'")
            .fetch_one(&pool)
            .await
            .unwrap();

        let cpu_usage: f64 = record.get("cpu_usage");
        assert_eq!(cpu_usage, 45.5);
    }

    #[tokio::test]
    async fn test_get_telemetry_groups_rows_by_hostname_and_filters_window() {
        let (app, pool) = build_test_app().await;
        let now = current_timestamp_sec();

        insert_test_telemetry(
            &pool,
            NodeTelemetry {
                hostname: "sbl1".to_string(),
                cpu_usage: 20.0,
                ram_used_mb: 4000,
                ram_total_mb: 16000,
                load_avg_1m: 0.5,
                load_avg_5m: 0.4,
                load_avg_15m: 0.3,
                uptime_secs: 100,
                disk_used_gb: 50.0,
                disk_total_gb: 100.0,
                tailscale_ip: "100.120.241.78".to_string(),
                timestamp_sec: now - 60,
            },
        )
        .await;

        insert_test_telemetry(
            &pool,
            NodeTelemetry {
                hostname: "sbl1".to_string(),
                cpu_usage: 21.0,
                ram_used_mb: 4100,
                ram_total_mb: 16000,
                load_avg_1m: 0.6,
                load_avg_5m: 0.5,
                load_avg_15m: 0.4,
                uptime_secs: 200,
                disk_used_gb: 51.0,
                disk_total_gb: 100.0,
                tailscale_ip: "100.120.241.78".to_string(),
                timestamp_sec: now - 30,
            },
        )
        .await;

        insert_test_telemetry(
            &pool,
            NodeTelemetry {
                hostname: "sbl2".to_string(),
                cpu_usage: 42.0,
                ram_used_mb: 2000,
                ram_total_mb: 8000,
                load_avg_1m: 1.2,
                load_avg_5m: 1.0,
                load_avg_15m: 0.8,
                uptime_secs: 300,
                disk_used_gb: 20.0,
                disk_total_gb: 80.0,
                tailscale_ip: "100.121.42.39".to_string(),
                timestamp_sec: now - 90,
            },
        )
        .await;

        insert_test_telemetry(
            &pool,
            NodeTelemetry {
                hostname: "sbl3".to_string(),
                cpu_usage: 99.0,
                ram_used_mb: 7000,
                ram_total_mb: 8000,
                load_avg_1m: 8.0,
                load_avg_5m: 7.0,
                load_avg_15m: 6.0,
                uptime_secs: 400,
                disk_used_gb: 70.0,
                disk_total_gb: 80.0,
                tailscale_ip: "100.88.237.49".to_string(),
                timestamp_sec: now - (3 * 60 * 60),
            },
        )
        .await;

        let req = Request::builder()
            .method(http::Method::GET)
            .uri("/telemetry?hours=2")
            .body(Body::empty())
            .unwrap();

        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let payload: TelemetryHistoryResponse = serde_json::from_slice(&body).unwrap();

        assert_eq!(payload.window_hours, 2);
        assert_eq!(payload.nodes.len(), 2);
        assert_eq!(payload.nodes["sbl1"].len(), 2);
        assert_eq!(payload.nodes["sbl2"].len(), 1);
        assert!(payload.nodes.get("sbl3").is_none());
        assert!(payload.nodes["sbl1"][0].timestamp_sec < payload.nodes["sbl1"][1].timestamp_sec);
    }

    async fn insert_test_telemetry(pool: &SqlitePool, payload: NodeTelemetry) {
        sqlx::query(
            r#"
            INSERT INTO telemetry
                (id, hostname, cpu_usage, ram_used_mb, ram_total_mb, load_avg_1m, load_avg_5m, load_avg_15m, uptime_secs, disk_used_gb, disk_total_gb, tailscale_ip, timestamp_sec)
            VALUES
                (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(Uuid::new_v4().to_string())
        .bind(payload.hostname)
        .bind(payload.cpu_usage)
        .bind(payload.ram_used_mb)
        .bind(payload.ram_total_mb)
        .bind(payload.load_avg_1m)
        .bind(payload.load_avg_5m)
        .bind(payload.load_avg_15m)
        .bind(payload.uptime_secs)
        .bind(payload.disk_used_gb)
        .bind(payload.disk_total_gb)
        .bind(payload.tailscale_ip)
        .bind(payload.timestamp_sec)
        .execute(pool)
        .await
        .unwrap();
    }
}
