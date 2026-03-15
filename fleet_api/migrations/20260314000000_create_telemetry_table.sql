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

CREATE INDEX IF NOT EXISTS idx_telemetry_hostname_timestamp ON telemetry (hostname, timestamp_sec DESC);
CREATE INDEX IF NOT EXISTS idx_telemetry_timestamp ON telemetry (timestamp_sec DESC);
