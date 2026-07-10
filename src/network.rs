//! Network I/O layer for the Rust smart gateway.
//!
//! Provides:
//! - UDP receiver on port 9999 (raw datagram → decode → hand off to router)
//! - WebSocket server on port 8080 (axum upgrade, Ping/Pong heartbeat)
//!
//! All wire-format parsing is delegated to the `gateway-protocol` crate.
//! This module **never** manually inspects byte offsets or re-defines
//! header structs — that is the single-source-of-truth contract from
//! `protocol.md` §4.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Query, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router as AxumRouter;
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use gateway_protocol::{DecodeError, PacketHeader, HEADER_LEN, PROTOCOL_VERSION};
use serde::Deserialize;
use tokio::net::UdpSocket;
use tokio::time::{interval, Duration, Instant};

use crate::routing::{self, Router};

// ---------------------------------------------------------------------------
// Metrics — simple atomic counters for observability
// ---------------------------------------------------------------------------

static UDP_PACKETS_RECEIVED: AtomicU64 = AtomicU64::new(0);
static UDP_DECODE_ERRORS: AtomicU64 = AtomicU64::new(0);
static WS_MESSAGES_RECEIVED: AtomicU64 = AtomicU64::new(0);
static WS_DECODE_ERRORS: AtomicU64 = AtomicU64::new(0);
static WS_CONNECTIONS_ACTIVE: AtomicU64 = AtomicU64::new(0);
static WS_PONG_TIMEOUTS: AtomicU64 = AtomicU64::new(0);

/// Snapshot of current metrics (for external exposure, e.g. health endpoint).
#[derive(Debug, Clone)]
pub struct Metrics {
    pub udp_packets_received: u64,
    pub udp_decode_errors: u64,
    pub ws_messages_received: u64,
    pub ws_decode_errors: u64,
    pub ws_connections_active: u64,
    pub ws_pong_timeouts: u64,
}

pub fn metrics() -> Metrics {
    Metrics {
        udp_packets_received: UDP_PACKETS_RECEIVED.load(Ordering::Relaxed),
        udp_decode_errors: UDP_DECODE_ERRORS.load(Ordering::Relaxed),
        ws_messages_received: WS_MESSAGES_RECEIVED.load(Ordering::Relaxed),
        ws_decode_errors: WS_DECODE_ERRORS.load(Ordering::Relaxed),
        ws_connections_active: WS_CONNECTIONS_ACTIVE.load(Ordering::Relaxed),
        ws_pong_timeouts: WS_PONG_TIMEOUTS.load(Ordering::Relaxed),
    }
}

// ---------------------------------------------------------------------------
// run_udp — UDP receive loop
// ---------------------------------------------------------------------------

/// Bind a UDP socket on `addr` (typically `0.0.0.0:9999`) and spawn a
/// dedicated background task that loops forever calling `recv_from`.
///
/// Each valid datagram is decoded via `PacketHeader::decode` and then handed
/// off to `router.route(…)` in a fresh `tokio::spawn` so the receive loop
/// never blocks.
///
/// This function returns immediately after spawning; the caller should `await`
/// a shutdown signal and then abort the returned handle.
pub async fn run_udp(router: Arc<Router>, addr: SocketAddr) {
    let socket = Arc::new(
        UdpSocket::bind(addr)
            .await
            .expect("failed to bind UDP socket"),
    );

    tracing::info!(%addr, "UDP receiver started");

    // Dedicated receive task — the loop body is intentionally tiny so the
    // socket is always ready to accept the next datagram.
    tokio::spawn(async move {
        let mut buf = vec![0u8; 65535];
        loop {
            let (n, _peer) = match socket.recv_from(&mut buf).await {
                Ok(v) => v,
                Err(e) => {
                    tracing::error!(error = %e, "UDP recv_from error");
                    continue;
                }
            };

            UDP_PACKETS_RECEIVED.fetch_add(1, Ordering::Relaxed);

            // Zero-copy: slice the receive buffer into a `Bytes`.
            let raw = Bytes::copy_from_slice(&buf[..n]);

            match PacketHeader::decode(raw) {
                Ok((header, payload)) => {
                    let router = Arc::clone(&router);
                    tokio::spawn(async move {
                        router.route(header, payload).await;
                    });
                }
                Err(e) => {
                    UDP_DECODE_ERRORS.fetch_add(1, Ordering::Relaxed);
                    record_decode_metric(&e);
                }
            }
        }
    });
}

// ---------------------------------------------------------------------------
// run_ws — axum WebSocket server
// ---------------------------------------------------------------------------

/// Shared application state handed into axum.
struct AppState {
    router: Arc<Router>,
}

/// Start the axum HTTP / WebSocket server on `addr` (typically
/// `0.0.0.0:8080`).
///
/// Blocks until the server exits (listener error or graceful shutdown).
pub async fn run_ws(router: Arc<Router>, addr: SocketAddr) {
    let state = Arc::new(AppState { router });

    let app = AxumRouter::new()
        .route("/ws", get(ws_handler))
        .fallback_service(tower_http::services::ServeDir::new("public"))
        .with_state(state);

    tracing::info!(%addr, "WebSocket server starting");

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("failed to bind TCP listener");

    axum::serve(listener, app)
        .await
        .expect("axum server fatal error");
}

// ---------------------------------------------------------------------------
// WS query parameter extraction
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct WsParams {
    room: String,
    user: String,
}

// ---------------------------------------------------------------------------
// WS upgrade handler
// ---------------------------------------------------------------------------

async fn ws_handler(
    ws: WebSocketUpgrade,
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Query(params): Query<WsParams>,
) -> impl IntoResponse {
    // Reject non-ASCII Room / User ID at connection stage (pre-upgrade).
    if !params.room.is_ascii() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            "room must be ASCII",
        )
            .into_response();
    }
    if !params.user.is_ascii() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            "user must be ASCII",
        )
            .into_response();
    }

    let room = params.room;
    let user = params.user;
    let router = Arc::clone(&state.router);

    ws.on_upgrade(move |socket| handle_ws_connection(router, socket, room, user))
}

// ---------------------------------------------------------------------------
// Per-connection task — read, write, ping/pong, register/unregister
// ---------------------------------------------------------------------------

/// Heartbeat constants.
const PING_INTERVAL: Duration = Duration::from_secs(10);
const PONG_TIMEOUT: Duration = Duration::from_secs(30);

/// Capacity of the per-connection outbound channel.  Must be large enough to
/// absorb bursts without blocking the router, but small enough to apply
/// back-pressure for `RawMotion` (oldest-eviction) and `SystemCmd` (timeout).
const WS_CHANNEL_CAPACITY: usize = 64;

async fn handle_ws_connection(
    router: Arc<Router>,
    ws: WebSocket,
    room: String,
    user: String,
) {
    let (mut ws_sender, mut ws_receiver) = ws.split();

    // Outbound channel: router pushes encoded `Bytes` through the sender;
    // this task drains the receiver and writes binary frames to the socket.
    let (tx, mut rx) = routing::bounded::<Bytes>(WS_CHANNEL_CAPACITY);

    // Register with the routing core.
    router.register(&room, &user, tx);
    WS_CONNECTIONS_ACTIVE.fetch_add(1, Ordering::Relaxed);

    tracing::info!(%room, %user, "WebSocket connection established");

    // Timestamp of the last sign of life (any message received from client).
    let mut last_alive = Instant::now();
    let mut ping_tick = interval(PING_INTERVAL);

    // First ping fires after the first interval (10 s), not immediately.
    ping_tick.reset();

    loop {
        tokio::select! {
            // ── incoming WebSocket message ────────────────────────────
            msg = ws_receiver.next() => {
                match msg {
                    Some(Ok(Message::Binary(data))) => {
                        last_alive = Instant::now();
                        WS_MESSAGES_RECEIVED.fetch_add(1, Ordering::Relaxed);

                        let raw = Bytes::from(data);
                        match PacketHeader::decode(raw) {
                            Ok((header, payload)) => {
                                let router = Arc::clone(&router);
                                tokio::spawn(async move {
                                    router.route(header, payload).await;
                                });
                            }
                            Err(e) => {
                                WS_DECODE_ERRORS.fetch_add(1, Ordering::Relaxed);
                                record_decode_metric(&e);
                            }
                        }
                    }
                    Some(Ok(Message::Pong(_))) => {
                        // Explicit client pong (in addition to any
                        // protocol-level auto-pong handled by tungstenite).
                        last_alive = Instant::now();
                    }
                    Some(Ok(Message::Ping(_))) => {
                        // Client ping — tungstenite auto-responds with a
                        // Pong; we just record liveness.
                        last_alive = Instant::now();
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        // Client sent close frame, or stream ended.
                        break;
                    }
                    Some(Ok(_text_or_other)) => {
                        // Text frames ignored; still counts as alive.
                        last_alive = Instant::now();
                    }
                    Some(Err(e)) => {
                        tracing::warn!(%room, %user, error = %e, "WebSocket read error");
                        break;
                    }
                }
            }

            // ── outbound data from router ─────────────────────────────
            data = rx.recv() => {
                match data {
                    Some(bytes) => {
                        if ws_sender
                            .send(Message::Binary(bytes))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    None => {
                        // All senders dropped — shut down this connection.
                        break;
                    }
                }
            }

            // ── periodic ping ─────────────────────────────────────────
            _ = ping_tick.tick() => {
                if ws_sender.send(Message::Ping(vec![].into())).await.is_err() {
                    break;
                }

                if last_alive.elapsed() > PONG_TIMEOUT {
                    WS_PONG_TIMEOUTS.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(
                        %room,
                        %user,
                        elapsed_ms = last_alive.elapsed().as_millis(),
                        "WebSocket pong timeout — disconnecting"
                    );
                    break;
                }
            }
        }
    }

    // ── cleanup ───────────────────────────────────────────────────────
    WS_CONNECTIONS_ACTIVE.fetch_sub(1, Ordering::Relaxed);
    router.unregister(&room, &user);

    // Best-effort close frame.
    let _ = ws_sender.send(Message::Close(None)).await;

    tracing::info!(%room, %user, "WebSocket connection closed");
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Record a decode error with the appropriate log level and message.
/// In a production system this would also increment a labelled Prometheus
/// counter.  Here we log at `WARN` level (all decode errors are actionable).
fn record_decode_metric(e: &DecodeError) {
    match e {
        DecodeError::TooShort => {
            tracing::warn!(
                "decode: packet too short (min {HEADER_LEN} B, protocol v{PROTOCOL_VERSION:#x})"
            );
        }
        DecodeError::UnsupportedVersion(v) => {
            tracing::warn!(version = v, "decode: unsupported protocol version");
        }
        DecodeError::NonZeroReserved(v) => {
            tracing::warn!(value = v, "decode: reserved field must be 0x00");
        }
        DecodeError::UnknownPacketType(v) => {
            tracing::warn!(raw_type = v, "decode: unknown packet type");
        }
        DecodeError::UnknownTargetType(v) => {
            tracing::warn!(raw_type = v, "decode: unknown target type");
        }
        DecodeError::NonAsciiRoomId => {
            tracing::warn!("decode: room_id contains non-ASCII bytes");
        }
        DecodeError::NonAsciiUserId => {
            tracing::warn!("decode: user_id contains non-ASCII bytes");
        }
        DecodeError::LengthMismatch { declared, actual } => {
            tracing::warn!(declared, actual, "decode: payload length mismatch");
        }
    }
}
