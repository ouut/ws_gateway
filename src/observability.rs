//! Task C — Observability.
//!
//! Structured logging (`tracing`), Prometheus metrics (`metrics` +
//! `metrics-exporter-prometheus`), and graceful shutdown via tokio signals.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use axum::routing::get;
use gateway_protocol::{DecodeError, PacketType, TargetType};
use metrics::{counter, describe_counter, describe_gauge, describe_histogram, gauge, histogram, Unit};
use metrics_exporter_prometheus::PrometheusBuilder;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

static LOGGING_INIT: AtomicBool = AtomicBool::new(false);
static METRICS_INIT: OnceLock<()> = OnceLock::new();

// ── Label helpers ──────────────────────────────────────────────────────────

fn packet_type_label(pt: PacketType) -> &'static str {
    match pt {
        PacketType::RawMotion => "RawMotion",
        PacketType::AiEvent => "AiEvent",
        PacketType::SystemCmd => "SystemCmd",
        PacketType::Heartbeat => "Heartbeat",
    }
}

fn target_type_label(tt: TargetType) -> &'static str {
    match tt {
        TargetType::Broadcast => "Broadcast",
        TargetType::Unicast => "Unicast",
    }
}

pub fn drop_reason_label(err: &DecodeError) -> &'static str {
    match err {
        DecodeError::TooShort => "too_short",
        DecodeError::UnsupportedVersion(_) => "unsupported_version",
        DecodeError::NonZeroReserved(_) => "non_zero_reserved",
        DecodeError::UnknownPacketType(_) => "unknown_packet_type",
        DecodeError::UnknownTargetType(_) => "unknown_target_type",
        DecodeError::NonAsciiRoomId => "non_ascii_room_id",
        DecodeError::NonAsciiUserId => "non_ascii_user_id",
        DecodeError::LengthMismatch { .. } => "length_mismatch",
    }
}

// ── Logging ─────────────────────────────────────────────────────────────────

pub fn init_logging() {
    if LOGGING_INIT.swap(true, Ordering::SeqCst) {
        return;
    }
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("gateway=info,warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_thread_ids(true)
        .init();
    info!("logging initialised");
}

// ── Structured log helpers ──────────────────────────────────────────────────

pub fn log_connection_established(room: &str, user: &str) {
    info!(%room, %user, "connection established");
}

pub fn log_connection_closed(room: &str, user: &str) {
    info!(%room, %user, "connection closed");
}

pub fn log_frame_dropped(reason: &str) {
    warn!("frame dropped: {reason}");
}

pub fn log_heartbeat_timeout(room: &str, user: &str) {
    warn!(%room, %user, "heartbeat timeout");
}

pub fn log_duplicate_connection(room: &str, user: &str) {
    warn!(%room, %user, "duplicate connection, kicking old session");
}

pub fn log_internal_error(err: &str) {
    error!("internal error: {err}");
}

// ── Metrics ─────────────────────────────────────────────────────────────────

pub fn init_metrics(addr: SocketAddr) {
    if METRICS_INIT.set(()).is_err() {
        return;
    }
    describe_gauge!(
        "active_connections_total",
        Unit::Count,
        "Current number of active connections, labelled by room"
    );
    describe_gauge!(
        "active_rooms_total",
        Unit::Count,
        "Current number of distinct rooms with at least one active connection"
    );
    describe_counter!(
        "packets_routed_total",
        Unit::Count,
        "Total number of packets successfully routed, labelled by packet_type and target_type"
    );
    describe_counter!(
        "packets_dropped_total",
        Unit::Count,
        "Total number of packets dropped, labelled by drop reason"
    );
    describe_histogram!(
        "route_latency_seconds",
        Unit::Seconds,
        "End-to-end route latency histogram in seconds"
    );

    let handle = PrometheusBuilder::new()
        .install_recorder()
        .expect("failed to install Prometheus metrics recorder");

    // Zero-initialise every metric so the Prometheus exporter emits them
    // even before any data has been recorded.
    gauge!("active_connections_total", "room" => "").set(0.0);
    gauge!("active_rooms_total").set(0.0);
    counter!("packets_routed_total", "packet_type" => "RawMotion", "target_type" => "Broadcast").increment(0);
    counter!("packets_dropped_total", "reason" => "init").increment(0);
    histogram!("route_latency_seconds").record(0.0);

    // Bind synchronously so the port is open before we return.
    let std_listener = std::net::TcpListener::bind(addr)
        .expect("failed to bind metrics TCP listener");
    std_listener.set_nonblocking(true).unwrap();
    let listener = tokio::net::TcpListener::from_std(std_listener).unwrap();

    tokio::spawn(async move {
        let app = axum::Router::new().route("/metrics", get(move || async move { handle.render() }));
        axum::serve(listener, app).await.unwrap();
    });
    info!("[metrics] Prometheus endpoint listening on http://{addr}/metrics");
}

// ── Metric-recording helpers ────────────────────────────────────────────────

pub fn metric_active_connections_set(room: &str, count: u64) {
    gauge!("active_connections_total", "room" => room.to_string()).set(count as f64);
}

pub fn metric_active_rooms_set(count: u64) {
    gauge!("active_rooms_total").set(count as f64);
}

pub fn metric_packet_routed(pt: PacketType, tt: TargetType) {
    counter!(
        "packets_routed_total",
        "packet_type" => packet_type_label(pt),
        "target_type" => target_type_label(tt),
    )
    .increment(1);
}

pub fn metric_packet_dropped(reason: &str) {
    counter!("packets_dropped_total", "reason" => reason.to_string()).increment(1);
}

pub fn metric_packet_dropped_decode(err: &DecodeError) {
    let reason = drop_reason_label(err);
    counter!("packets_dropped_total", "reason" => reason).increment(1);
}

pub fn metric_route_latency(secs: f64) {
    histogram!("route_latency_seconds").record(secs);
}

// ── Graceful shutdown ───────────────────────────────────────────────────────

pub async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl-C handler");
        info!("received SIGINT (Ctrl-C)");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
        info!("received SIGTERM");
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    info!("shutdown signal received — draining connections");
}

pub async fn graceful_shutdown_with_drain<F>(drain_timeout: Duration, drain: F)
where
    F: std::future::Future<Output = ()>,
{
    shutdown_signal().await;

    info!(
        "draining for up to {}.{} s",
        drain_timeout.as_secs(),
        drain_timeout.subsec_millis()
    );

    tokio::select! {
        _ = drain => {
            info!("drain complete — exiting");
        }
        _ = tokio::time::sleep(drain_timeout) => {
            warn!("drain timeout reached — forcing exit");
        }
    }

    std::process::exit(0);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drop_reason_label_covers_all_variants() {
        let variants: &[DecodeError] = &[
            DecodeError::TooShort,
            DecodeError::UnsupportedVersion(0x99),
            DecodeError::NonZeroReserved(0xFF),
            DecodeError::UnknownPacketType(0xFE),
            DecodeError::UnknownTargetType(0xFD),
            DecodeError::NonAsciiRoomId,
            DecodeError::NonAsciiUserId,
            DecodeError::LengthMismatch {
                declared: 10,
                actual: 5,
            },
        ];
        let mut labels = std::collections::HashSet::new();
        for v in variants {
            let label = drop_reason_label(v);
            assert!(!label.is_empty());
            assert!(labels.insert(label), "duplicate label: {label}");
        }
    }

    #[test]
    fn packet_type_labels_are_unique() {
        let types = &[
            PacketType::RawMotion,
            PacketType::AiEvent,
            PacketType::SystemCmd,
            PacketType::Heartbeat,
        ];
        let mut labels = std::collections::HashSet::new();
        for t in types {
            let label = packet_type_label(*t);
            assert!(!label.is_empty());
            assert!(labels.insert(label), "duplicate label: {label}");
        }
    }

    #[test]
    fn target_type_labels_are_unique() {
        let types = &[TargetType::Broadcast, TargetType::Unicast];
        let mut labels = std::collections::HashSet::new();
        for t in types {
            let label = target_type_label(*t);
            assert!(!label.is_empty());
            assert!(labels.insert(label), "duplicate label: {label}");
        }
    }

    #[test]
    fn logging_init_is_idempotent() {
        // Must not panic on repeated calls.
        init_logging();
        init_logging();
        init_logging();
    }
}
