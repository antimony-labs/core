use serde::{Deserialize, Serialize};
use sqlx::FromRow;

#[derive(Debug, Serialize, Deserialize, Clone, FromRow)]
pub struct NodeTelemetry {
    pub hostname: String,
    pub cpu_usage: f32,
    pub ram_used_mb: i64,
    pub ram_total_mb: i64,
    pub load_avg_1m: f32,
    pub load_avg_5m: f32,
    pub load_avg_15m: f32,
    pub uptime_secs: i64,
    pub disk_used_gb: f32,
    pub disk_total_gb: f32,
    pub tailscale_ip: String,
    pub timestamp_sec: i64,
}
