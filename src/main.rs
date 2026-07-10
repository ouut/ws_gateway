//! Gateway — main entry point.
//!
//! Topology:
//!   init_logging() + init_metrics(:9090)
//!   → Router::new()
//!   → spawn run_udp(router)  (:9999)
//!   → spawn run_ws(router)   (:8080, WS + static files)
//!   → shutdown_signal().await

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use gateway_protocol::{PacketHeader, PacketType, TargetType};
use tracing::info;

mod routing;
mod network;
mod observability;
mod static_files;

use observability::{init_logging, init_metrics, shutdown_signal};
use routing::Router;

#[derive(Parser, Debug)]
#[command(name = "gateway", version, about = "Rust Smart Gateway — binary protocol data bus")]
struct Cli {
    /// UDP listen address (default: 0.0.0.0:9999)
    #[arg(long, default_value = "0.0.0.0:9999")]
    udp: SocketAddr,

    /// WebSocket + HTTP listen address (default: 0.0.0.0:8080)
    #[arg(long, default_value = "0.0.0.0:8080")]
    ws: SocketAddr,

    /// Prometheus metrics listen address (default: 0.0.0.0:9090)
    #[arg(long, default_value = "0.0.0.0:9090")]
    metrics: SocketAddr,

    /// Static files directory (default: ./public)
    #[arg(long, default_value = "./public")]
    public_dir: PathBuf,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    init_logging();
    info!("gateway starting — protocol v{}", gateway_protocol::PROTOCOL_VERSION);
    info!("udp={} ws={} metrics={} public={}", cli.udp, cli.ws, cli.metrics, cli.public_dir.display());

    // Type-consistency sentinel
    let _ = PacketHeader::new(PacketType::Heartbeat, TargetType::Broadcast, "sntnl", "sentinel", 0, 0);

    init_metrics(cli.metrics);

    let router = Arc::new(Router::new());
    info!("router initialised");

    let udp_handle = tokio::spawn(network::run_udp(Arc::clone(&router), cli.udp));
    info!("udp task spawned on {}", cli.udp);

    let ws_handle = tokio::spawn(network::run_ws(Arc::clone(&router), cli.ws, cli.public_dir));
    info!("ws + static server spawned on {}", cli.ws);

    shutdown_signal().await;

    info!("shutting down…");
    udp_handle.abort();
    ws_handle.abort();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    info!("gateway stopped");
}
