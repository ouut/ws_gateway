//! Gateway — main entry point (Task E assembly).
//!
//! Topology:
//!   init_logging() + init_metrics(:9090)
//!   → Router::new()
//!   → spawn run_udp(router)  (:9999)
//!   → spawn run_ws(router)   (:8080, WS + static files)
//!   → shutdown_signal().await

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use gateway_protocol::{PacketHeader, PacketType, TargetType};
use tracing::info;

use gateway::observability::{init_logging, init_metrics, shutdown_signal};
use gateway::routing::Router;

const UDP_ADDR: SocketAddr = SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::UNSPECIFIED), 9999);
const WS_ADDR: SocketAddr  = SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::UNSPECIFIED), 8080);
const METRICS_ADDR: SocketAddr = SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::UNSPECIFIED), 9090);

#[tokio::main]
async fn main() {
    init_logging();
    info!("gateway starting — protocol v{}", gateway_protocol::PROTOCOL_VERSION);

    // Type-consistency sentinel
    let _ = PacketHeader::new(PacketType::Heartbeat, TargetType::Broadcast, "sntnl", "sentinel", 0, 0);

    init_metrics(METRICS_ADDR);

    let router = Arc::new(Router::new());
    info!("router initialised");

    let udp_handle = tokio::spawn(gateway::network::run_udp(Arc::clone(&router), UDP_ADDR));
    info!("udp task spawned on {UDP_ADDR}");

    let ws_handle = tokio::spawn(gateway::network::run_ws(Arc::clone(&router), WS_ADDR));
    info!("ws + static server spawned on {WS_ADDR}");

    shutdown_signal().await;

    info!("shutting down…");
    udp_handle.abort();
    ws_handle.abort();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    info!("gateway stopped");
}
