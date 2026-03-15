use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct NodeTelemetry {
    pub hostname: String,
    pub cpu_usage: f32,
    pub ram_used_mb: u64,
    pub ram_total_mb: u64,
    pub tailscale_ip: String,
    pub timestamp_sec: i64,
}
