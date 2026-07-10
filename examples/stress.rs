//! Stress test — spawn N concurrent WebSocket clients, send M messages
//! each, measure round-trip latency and throughput.
//!
//! Usage:
//!   cargo run --example stress -- [--clients 100] [--messages 50] [--ws 8080]
//!
//! The gateway binary must already be running on the specified WS port.

use std::net::TcpStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "stress")]
struct Cli {
    /// Number of concurrent WS clients
    #[arg(long, default_value = "50")]
    clients: usize,

    /// Messages per client
    #[arg(long, default_value = "20")]
    messages: usize,

    /// Gateway WS port
    #[arg(long, default_value = "8080")]
    ws: u16,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let base = format!("ws://127.0.0.1:{}/ws?room=bench&user=", cli.ws);

    println!("╔══════════════════════════════════════════════╗");
    println!("║  Gateway Stress Test                         ║");
    println!("╠══════════════════════════════════════════════╣");
    println!("║  clients : {:>5}                              ║", cli.clients);
    println!("║  msg/conn: {:>5}                              ║", cli.messages);
    println!("║  total   : {:>5} messages                     ║", cli.clients * cli.messages);
    println!("╚══════════════════════════════════════════════╝\n");

    // ── Connect all clients concurrently ─────────────────────────────────
    let connect_start = Instant::now();
    let mut tasks = Vec::with_capacity(cli.clients);

    for i in 0..cli.clients {
        let url_str = format!("{base}{i}");
        tasks.push(tokio::spawn(async move {
            let (ws, _) = match tokio_tungstenite::connect_async(&url_str).await {
                Ok(c) => c,
                Err(e) => panic!("client {i}: connect failed: {e}"),
            };
            (i, ws)
        }));
    }

    let mut clients = Vec::with_capacity(cli.clients);
    for t in tasks {
        clients.push(t.await.unwrap());
    }
    let connect_ms = connect_start.elapsed().as_millis();
    println!("connect : {cli_clients} clients in {connect_ms} ms\n", cli_clients = cli.clients);

    // ── Warm-up round ────────────────────────────────────────────────────
    for (i, ws) in clients.iter_mut() {
        let wire = make_packet(&format!("warm{}", i), b"warm");
        ws.send(tokio_tungstenite::tungstenite::Message::Binary(wire))
            .await
            .unwrap();
    }
    for (_, ws) in clients.iter_mut() {
        let _ = ws.next().await; // drain
    }

    // ── Measure ───────────────────────────────────────────────────────────
    let mut latencies_us = Vec::with_capacity(cli.clients * cli.messages);
    let bench_start = Instant::now();

    for round in 0..cli.messages {
        let payload = format!("msg{round}").into_bytes();

        // Send all clients (fire-and-forget).
        for (i, ws) in clients.iter_mut() {
            let wire = make_packet(&format!("user{}", i), &payload);
            ws.send(tokio_tungstenite::tungstenite::Message::Binary(wire))
                .await
                .unwrap();
        }

        // Receive all clients, measuring individual latency.
        for (_, ws) in clients.iter_mut() {
            let t0 = Instant::now();
            match ws.next().await {
                Some(Ok(_)) => {
                    latencies_us.push(t0.elapsed().as_micros() as f64);
                }
                None => panic!("client disconnected"),
                Some(Err(e)) => panic!("client error: {e}"),
            }
        }
    }

    let total_ms = bench_start.elapsed().as_millis();
    let total_msg = cli.clients * cli.messages;
    let throughput = total_msg as f64 / (total_ms as f64 / 1000.0);

    // ── Statistics ───────────────────────────────────────────────────────
    latencies_us.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = latencies_us.len();

    let p50 = latencies_us[n / 2];
    let p90 = latencies_us[(n as f64 * 0.90) as usize];
    let p99 = latencies_us[(n as f64 * 0.99) as usize];
    let p999 = latencies_us[(n as f64 * 0.999) as usize];
    let min = latencies_us[0];
    let max = latencies_us[n - 1];
    let avg = latencies_us.iter().sum::<f64>() / n as f64;

    // ── Report ───────────────────────────────────────────────────────────
    println!("╔══════════════════════════════════════════════╗");
    println!("║  Results                                     ║");
    println!("╠══════════════════════════════════════════════╣");
    println!("║  duration  : {total_ms:>5} ms                       ║");
    println!("║  messages  : {total_msg:>5}                          ║");
    println!("║  throughput: {throughput:>8.0} msg/s                   ║");
    println!("╠══════════════════════════════════════════════╣");
    println!("║  Latency (round-trip, µs)                    ║");
    println!("╠══════════════════════════════════════════════╣");
    println!("║  min : {:>8.0}                            ║", min);
    println!("║  avg : {:>8.0}                            ║", avg);
    println!("║  P50 : {:>8.0}                            ║", p50);
    println!("║  P90 : {:>8.0}                            ║", p90);
    println!("║  P99 : {:>8.0}                            ║", p99);
    println!("║  P999: {:>8.0}                            ║", p999);
    println!("║  max : {:>8.0}                            ║", max);
    println!("╚══════════════════════════════════════════════╝");

    // ── Cleanup ──────────────────────────────────────────────────────────
    for (_, ws) in clients.iter_mut() {
        ws.close(None).await.ok();
    }
}

fn make_packet(user: &str, payload: &[u8]) -> bytes::Bytes {
    gateway_protocol::PacketHeader::new(
        gateway_protocol::PacketType::AiEvent,
        gateway_protocol::TargetType::Broadcast,
        "bench",
        user,
        0,
        payload.len(),
    )
    .unwrap()
    .encode(payload)
}
