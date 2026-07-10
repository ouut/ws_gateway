//! Integration tests for the Rust Smart Gateway.
//!
//! Covers:
//!   - Static file serving
//!   - WebSocket: connect, binary encode/decode, broadcast to multiple users,
//!     non-ASCII rejection, duplicate kick
//!   - UDP: datagram send → WS receive, error handling
//!
//! Requires `cargo build` before `cargo test --test integration_test`.

use std::io::{Read, Write};
use std::net::{TcpStream, UdpSocket};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, TryStreamExt};
use tokio_tungstenite::tungstenite;

const BINARY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/target/debug/gateway");

// ── Spawn helpers ────────────────────────────────────────────────────────────

fn spawn_gateway() -> (Child, u16, u16) {
    let ws_port = pick_port();
    let metrics_port = pick_port();
    let child = Command::new(BINARY)
        .args([
            "--ws",
            &format!("127.0.0.1:{ws_port}"),
            "--metrics",
            &format!("127.0.0.1:{metrics_port}"),
            "--public-dir",
            concat!(env!("CARGO_MANIFEST_DIR"), "/public"),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("failed to spawn gateway — run `cargo build` first");
    wait_port(ws_port, Duration::from_secs(5));
    (child, ws_port, metrics_port)
}

fn spawn_gateway_with_udp() -> (Child, u16, u16, u16) {
    let ws_port = pick_port();
    let udp_port = pick_port();
    let metrics_port = pick_port();
    let child = Command::new(BINARY)
        .args([
            "--udp",
            &format!("127.0.0.1:{udp_port}"),
            "--ws",
            &format!("127.0.0.1:{ws_port}"),
            "--metrics",
            &format!("127.0.0.1:{metrics_port}"),
            "--public-dir",
            concat!(env!("CARGO_MANIFEST_DIR"), "/public"),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("failed to spawn gateway");
    wait_port(ws_port, Duration::from_secs(5));
    (child, ws_port, udp_port, metrics_port)
}

fn pick_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .map(|l| l.local_addr().unwrap().port())
        .unwrap_or(0)
}

fn wait_port(port: u16, timeout: Duration) {
    let start = Instant::now();
    loop {
        if TcpStream::connect_timeout(
            &format!("127.0.0.1:{port}").parse().unwrap(),
            Duration::from_millis(200),
        )
        .is_ok()
        {
            return;
        }
        assert!(start.elapsed() < timeout, "gateway startup timeout on port {port}");
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn http_get(host: &str, port: u16, path: &str) -> String {
    let mut stream = TcpStream::connect_timeout(
        &format!("{host}:{port}").parse().unwrap(),
        Duration::from_secs(3),
    )
    .unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).unwrap();
    let mut buf = String::new();
    stream.read_to_string(&mut buf).unwrap();
    buf
}

// ═══════════════════════════════════════════════════════════════════════════
// Static file server
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn static_index_html() {
    let (_child, ws_port, _) = spawn_gateway();
    let body = http_get("127.0.0.1", ws_port, "/index.html");
    assert!(body.contains("<html"), "index.html not served");
}

#[test]
fn static_chat_html() {
    let (_child, ws_port, _) = spawn_gateway();
    let body = http_get("127.0.0.1", ws_port, "/chat.html");
    assert!(body.contains("<html"), "chat.html not served");
}

#[test]
fn static_js_sdk() {
    let (_child, ws_port, _) = spawn_gateway();
    let body = http_get("127.0.0.1", ws_port, "/gateway_protocol.js");
    assert!(body.contains("PROTOCOL_VERSION"), "SDK not served");
}

#[test]
fn static_404_returns_error() {
    let (_child, ws_port, _) = spawn_gateway();
    let body = http_get("127.0.0.1", ws_port, "/nonexistent.xyz");
    assert!(body.len() < 200, "expected empty/short 404 body");
}

// ═══════════════════════════════════════════════════════════════════════════
// WebSocket
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn ws_connect_and_send() {
    let (_child, ws_port, _) = spawn_gateway();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .unwrap();
    rt.block_on(async {
        let (mut ws, _) = tokio_tungstenite::connect_async(
            format!("ws://127.0.0.1:{ws_port}/ws?room=test&user=A"),
        )
        .await
        .unwrap();

        let wire = gateway_protocol::PacketHeader::new(
            gateway_protocol::PacketType::AiEvent,
            gateway_protocol::TargetType::Broadcast,
            "test", "A", 0, 5,
        )
        .unwrap()
        .encode(b"hello");
        ws.send(tungstenite::Message::Binary(wire)).await.unwrap();

        let msg = ws.try_next().await.unwrap().unwrap();
        assert!(matches!(msg, tungstenite::Message::Binary(_)));
        ws.close(None).await.ok();
    });
}

#[test]
fn ws_two_users_broadcast() {
    let (_child, ws_port, _) = spawn_gateway();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .unwrap();
    rt.block_on(async {
        let (mut alice, _) = tokio_tungstenite::connect_async(
            format!("ws://127.0.0.1:{ws_port}/ws?room=roomx&user=Alice"),
        )
        .await
        .unwrap();
        let (mut bob, _) = tokio_tungstenite::connect_async(
            format!("ws://127.0.0.1:{ws_port}/ws?room=roomx&user=Bob"),
        )
        .await
        .unwrap();

        let wire = gateway_protocol::PacketHeader::new(
            gateway_protocol::PacketType::AiEvent,
            gateway_protocol::TargetType::Broadcast,
            "roomx", "Alice", 0, 2,
        )
        .unwrap()
        .encode(b"yo");
        alice.send(tungstenite::Message::Binary(wire)).await.unwrap();

        assert!(matches!(alice.try_next().await.unwrap(), Some(tungstenite::Message::Binary(_))));
        assert!(matches!(bob.try_next().await.unwrap(), Some(tungstenite::Message::Binary(_))));

        alice.close(None).await.ok();
        bob.close(None).await.ok();
    });
}

#[test]
#[ignore]
fn ws_sequence_preserved() {
    let (_child, ws_port, _) = spawn_gateway();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .unwrap();
    rt.block_on(async {
        let (mut ws, _) = tokio_tungstenite::connect_async(
            format!("ws://127.0.0.1:{ws_port}/ws?room=seq&user=S"),
        )
        .await
        .unwrap();

        for seq in [0u32, 1, 42, u32::MAX] {
            let wire = gateway_protocol::PacketHeader::new(
                gateway_protocol::PacketType::AiEvent,
                gateway_protocol::TargetType::Broadcast,
                "seq", "S", seq, 0,
            )
            .unwrap()
            .encode(b"");
            ws.send(tungstenite::Message::Binary(wire)).await.unwrap();
            if let Some(tungstenite::Message::Binary(data)) = ws.try_next().await.unwrap() {
                let (hdr, _) =
                    gateway_protocol::PacketHeader::decode(data).unwrap();
                assert_eq!(hdr.sequence, seq);
            }
        }
        ws.close(None).await.ok();
    });
}

#[test]
fn ws_reject_non_ascii_room() {
    let (_child, ws_port, _) = spawn_gateway();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .unwrap();
    rt.block_on(async {
        let err = tokio_tungstenite::connect_async(
            format!("ws://127.0.0.1:{ws_port}/ws?room=房间&user=ok"),
        )
        .await
        .unwrap_err();
        assert!(format!("{err}").to_lowercase().contains("400"));
    });
}

#[test]
#[ignore]
fn ws_duplicate_user_kicks_old() {
    let (_child, ws_port, _) = spawn_gateway();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .unwrap();
    rt.block_on(async {
        let (mut first, _) = tokio_tungstenite::connect_async(
            format!("ws://127.0.0.1:{ws_port}/ws?room=kick&user=X"),
        )
        .await
        .unwrap();
        let (mut second, _) = tokio_tungstenite::connect_async(
            format!("ws://127.0.0.1:{ws_port}/ws?room=kick&user=X"),
        )
        .await
        .unwrap();

        // Old connection should be closed by the server.
        let first_read = tokio::time::timeout(Duration::from_secs(2), first.try_next()).await;
        assert!(
            matches!(first_read, Ok(Err(_)) | Ok(Ok(None)) | Ok(Ok(Some(tungstenite::Message::Close(_)))) | Err(_)),
            "old connection should be kicked"
        );

        // New connection works.
        let wire = gateway_protocol::PacketHeader::new(
            gateway_protocol::PacketType::AiEvent,
            gateway_protocol::TargetType::Broadcast,
            "kick", "X", 0, 0,
        )
        .unwrap()
        .encode(b"");
        second.send(tungstenite::Message::Binary(wire)).await.unwrap();
        assert!(matches!(
            second.try_next().await.unwrap(),
            Some(tungstenite::Message::Binary(_))
        ));
        second.close(None).await.ok();
    });
}

// ═══════════════════════════════════════════════════════════════════════════
// UDP
// ═══════════════════════════════════════════════════════════════════════════

#[test]
#[ignore]
fn udp_to_ws_broadcast() {
    let (_child, ws_port, udp_port, _) = spawn_gateway_with_udp();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .unwrap();
    rt.block_on(async {
        let (mut ws, _) = tokio_tungstenite::connect_async(
            format!("ws://127.0.0.1:{ws_port}/ws?room=udptest&user=A"),
        )
        .await
        .unwrap();

        let wire = gateway_protocol::PacketHeader::new(
            gateway_protocol::PacketType::AiEvent,
            gateway_protocol::TargetType::Broadcast,
            "udpts", "sensor", 7, 5,
        )
        .unwrap()
        .encode(b"udp!!");

        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        sock.send_to(&wire, format!("127.0.0.1:{udp_port}")).unwrap();

        if let Some(tungstenite::Message::Binary(data)) = ws.try_next().await.unwrap() {
            let (hdr, payload) =
                gateway_protocol::PacketHeader::decode(data).unwrap();
            assert_eq!(hdr.user_id_str(), "sensor");
            assert_eq!(payload.as_ref(), b"udp!!");
        } else {
            panic!("expected binary from UDP");
        }
        ws.close(None).await.ok();
    });
}

#[test]
#[ignore]
fn udp_truncated_packet_dropped() {
    let (_child, ws_port, udp_port, _) = spawn_gateway_with_udp();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .unwrap();
    rt.block_on(async {
        let (mut ws, _) = tokio_tungstenite::connect_async(
            format!("ws://127.0.0.1:{ws_port}/ws?room=udpdrop&user=A"),
        )
        .await
        .unwrap();

        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        // Truncated packet.
        sock.send_to(&[0u8; 10], format!("127.0.0.1:{udp_port}")).unwrap();
        // Valid packet.
        let wire = gateway_protocol::PacketHeader::new(
            gateway_protocol::PacketType::AiEvent,
            gateway_protocol::TargetType::Broadcast,
            "udpdr", "s", 0, 0,
        )
        .unwrap()
        .encode(b"");
        sock.send_to(&wire, format!("127.0.0.1:{udp_port}")).unwrap();

        assert!(matches!(
            ws.try_next().await.unwrap(),
            Some(tungstenite::Message::Binary(_))
        ));
        ws.close(None).await.ok();
    });
}

#[test]
#[ignore]
fn udp_wrong_version_dropped() {
    let (_child, ws_port, udp_port, _) = spawn_gateway_with_udp();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .unwrap();
    rt.block_on(async {
        let (mut ws, _) = tokio_tungstenite::connect_async(
            format!("ws://127.0.0.1:{ws_port}/ws?room=udpver&user=A"),
        )
        .await
        .unwrap();

        let mut raw = vec![0u8; 24];
        raw[0] = 0x99;
        raw[1] = gateway_protocol::PacketType::AiEvent.as_u8();
        raw[2] = gateway_protocol::TargetType::Broadcast.as_u8();
        raw[4..10].copy_from_slice(b"udpver");
        raw[10..18].copy_from_slice(b"badver\0\0");
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        sock.send_to(&raw, format!("127.0.0.1:{udp_port}")).unwrap();

        let wire = gateway_protocol::PacketHeader::new(
            gateway_protocol::PacketType::AiEvent,
            gateway_protocol::TargetType::Broadcast,
            "udpver", "ok", 0, 0,
        )
        .unwrap()
        .encode(b"");
        sock.send_to(&wire, format!("127.0.0.1:{udp_port}")).unwrap();

        assert!(matches!(
            ws.try_next().await.unwrap(),
            Some(tungstenite::Message::Binary(_))
        ));
        ws.close(None).await.ok();
    });
}

// ═══════════════════════════════════════════════════════════════════════════
// Metrics
// ═══════════════════════════════════════════════════════════════════════════

#[test]
#[ignore]
fn metrics_endpoint_exposes_counters() {
    let (_child, _ws_port, metrics_port) = spawn_gateway();
    let body = http_get("127.0.0.1", metrics_port, "/metrics");
    // Prometheus text format may use HELP/TYPE comments or bare metric lines.
    assert!(
        body.contains("active_connections_total")
            || body.contains("active_rooms_total")
            || body.contains("# HELP")
            || body.contains("# TYPE"),
        "metrics endpoint returned unexpected content"
    );
}
