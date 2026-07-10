# Rust Smart Gateway (ws_gateway)

A single-binary, zero-dependency, high-performance binary data bus for real-time message routing between motion sensors, web frontends, and AI prediction nodes.

## Architecture

```
                    ┌────────────────────────────────────────┐
                    │           Rust Gateway (tokio)          │
   [Sensor / UDP] ─►│  UDP Receiver (:9999)                  │
                    │        │                                │
                    │        ▼                                │
                    │  Routing Core (DashMap, sharded locks)  │
                    │        ▲                                │
                    │        │                                │
   [Frontend/AI/WS]◄►│  WS read/write (:8080) + heartbeat     │
                    │  Static file server (:8080)             │
                    │  Metrics / Logging (:9090/metrics)      │
                    └────────────────────────────────────────┘
```

- **Everyone is a client**: motion sensors, web frontends, and AI nodes are all equal peers.
- **Binary end-to-end**: 24-byte fixed-length header + payload, zero-copy routing.
- **Minimal business intrusion**: the gateway only parses header fields for routing, never inspects payload.

## Quick Start

### Download

Download the binary for your platform from [Releases](https://github.com/ouut/ws_gateway/releases):

| Platform | File | Notes |
|---|---|---|
| Linux x86_64 | `gateway-linux-x86_64` | |
| Linux ARM64 | `gateway-linux-aarch64` | Raspberry Pi, ARM servers |
| Windows x86_64 | `gateway-windows-x86_64.exe` | |
| macOS (Intel / Apple Silicon) | see build section below | No prebuilt binary; build locally |

### Run

```bash
# Linux
chmod +x gateway-linux-x86_64
./gateway-linux-x86_64

# macOS (build locally)
cargo build --release && ./target/release/gateway

# Windows
gateway-windows-x86_64.exe
```

**Listening ports:**

| Port | Protocol | Purpose |
|---|---|---|
| 8080 | HTTP / WebSocket | WS connections + static files |
| 9999 | UDP | Sensor data ingestion |
| 9090 | HTTP | Prometheus metrics |

### Build from source

**Prerequisites:** Rust 1.70+

```bash
git clone https://github.com/ouut/ws_gateway.git
cd ws_gateway

# Linux / macOS — one command
cargo build --release && ./target/release/gateway
```

#### macOS build (Intel / Apple Silicon)

Rust auto-detects the native architecture — no extra configuration needed:

```bash
# Install Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Build (auto-targets Intel x86_64 or Apple Silicon aarch64)
cd ws_gateway
cargo build --release
./target/release/gateway
```

#### Cross-compile (Linux → Windows / ARM)

```bash
# Install cross-compilation toolchains (Debian/Ubuntu)
apt-get install gcc-mingw-w64-x86-64 gcc-aarch64-linux-gnu

# One-command build + package for Linux x86_64 / ARM64 / Windows
./scripts/build.sh
```

> Cross-compiling for macOS requires Apple SDK (osxcross). Build natively on a Mac instead. The `gateway-protocol` library can be cross-compiled to macOS from Linux.

## Client Integration

### WebSocket (browser / AI client)

```
ws://host:8080/ws?room=<ROOM>&user=<USER>
```

Parameters:
- `room` — Room ID, max 6 ASCII bytes
- `user` — User ID, max 8 ASCII bytes
- No authentication (trusted LAN only)

**Example:**

```javascript
const ws = new WebSocket('ws://localhost:8080/ws?room=ninja1&user=player_A');
ws.binaryType = 'arraybuffer';

// Receive
ws.onmessage = (event) => {
    const data = new Uint8Array(event.data);
    // Parse 24-byte header + payload with gateway_protocol.js
    const packet = gatewayProtocol.decode(data);
    console.log(packet.roomId, packet.payload);
};

// Send
ws.send(encodedBytes);
```

### JavaScript SDK

```javascript
import { encode, decode, PKT_AI_EVENT, TGT_UNICAST } from './gateway_protocol.js';

const wire = encode({
    version: 0x02,
    pktType: PKT_AI_EVENT,
    tgtType: TGT_UNICAST,
    roomId: 'ninja1',
    userId: 'ai_bot',
    seq: 42,
    payload: new Uint8Array([0x02]),
});

ws.send(wire);
```

### Python SDK

```python
from gateway_protocol import encode, decode, PKT_AI_EVENT, TGT_BROADCAST

wire = encode(
    version=0x02, pkt_type=PKT_AI_EVENT, tgt_type=TGT_BROADCAST,
    room_id="ninja1", user_id="ai_bot", seq=42,
    payload=b'\x02'
)
# wire can be written directly to a WebSocket or UDP socket
```

## Protocol

24-byte fixed-length binary header (big-endian):

```
Offset:0      1        2        3        4         10        18         22   24
     +------+--------+--------+--------+---------+---------+----------+------+
     |Ver(1)|PktTy(1)|TgtTy(1)|Resv(1) |RoomID(6)|UserID(8)|Seq(4,BE)|Len(2,BE)|
     +------+--------+--------+--------+---------+---------+----------+------+
```

| Field | Offset | Size | Description |
|---|---|---|---|
| Version | 0 | 1B | `0x02` |
| Packet Type | 1 | 1B | `0x01` RawMotion / `0x02` AiEvent / `0x03` SystemCmd / `0x04` Heartbeat |
| Target Type | 2 | 1B | `0x01` Broadcast / `0x02` Unicast |
| Reserved | 3 | 1B | Must be `0x00` |
| Room ID | 4 | 6B | Fixed-length ASCII, right-padded with `\0` |
| User ID | 10 | 8B | Same as above |
| Sequence | 18 | 4B | u32 BE, monotonically increasing per sender |
| Length | 22 | 2B | u16 BE, payload byte count |

See [protocol.md](doc/gateway-protocol/protocol.md) for full details.

## Configuration

Default values (currently hardcoded):

| Variable | Default | Description |
|---|---|---|
| `UDP_PORT` | 9999 | UDP listen port |
| `WS_PORT` | 8080 | WebSocket + static files port |
| `METRICS_PORT` | 9090 | Prometheus metrics port |
| `PING_INTERVAL` | 10s | WS heartbeat interval |
| `PONG_TIMEOUT` | 30s | WS heartbeat timeout |
| `CHANNEL_CAPACITY` | 64 | Per-connection send queue capacity |

Log level: `RUST_LOG=gateway=info`

## Metrics

Visit `http://host:9090/metrics` for Prometheus-format metrics:

| Metric | Description |
|---|---|
| `active_connections_total` | Active connections (labeled by room) |
| `active_rooms_total` | Active rooms |
| `packets_routed_total` | Packets routed (labeled by Packet Type / Target Type) |
| `packets_dropped_total` | Packets dropped (labeled by drop reason) |
| `route_latency_seconds` | Route latency histogram |

## Testing

```bash
# Full test suite (32 tests)
cargo test -- --test-threads=1

# Protocol crate only
cargo test --manifest-path doc/gateway-protocol/Cargo.toml

# JS SDK
node client_sdk/js/gateway_protocol.js
```

### Unit tests (19)

| Module | Count | Coverage |
|---|---|---|
| `routing.rs` | 10 | Channel, broadcast, unicast, duplicate kick, backpressure |
| `network.rs` | 5 | Decode metric, heartbeat constants, channel capacity |
| `observability.rs` | 4 | Label uniqueness, logging idempotency |

### Integration tests (13)

| Category | Tests |
|---|---|
| Static files | index, chat, JS SDK, 404 |
| WebSocket | connect/send, two-user broadcast, sequence preserved, non-ASCII reject, duplicate kick |
| UDP | broadcast to WS, truncated drop, wrong version drop |
| Metrics | Prometheus endpoint |

### Stress test

```bash
# Start gateway
cargo run

# Run benchmark (in another terminal)
cargo run --example stress --release -- --clients 100 --messages 50 --ws 8080
```

Results (localhost, 200 clients × 50 msgs, release build):

| Metric | Value |
|---|---|
| Throughput | 108,696 msg/s |
| P50 latency | 0 µs |
| P90 latency | 1 µs |
| **P99 latency** | **29 µs** |
| P999 latency | 46 µs |
| Max latency | 207 µs |

> PRD target: P99 < 500 µs. Measured P99: 29 µs — **17× better** than target.

## Security

This gateway performs **no authentication and no data integrity checks**. It is designed for **trusted LAN environments only**.

- ❌ Never expose `:8080` (WS) or `:9999` (UDP) to the public internet
- ❌ Corrupted payloads are not detected — downstream consumers are responsible
- ✅ In production, place nginx/CDN in front for static assets; the gateway handles WS and UDP only

## Project Structure

```
ws_gateway/
├── src/                           # Gateway source
│   ├── main.rs                    # Entry point + CLI args
│   ├── routing.rs                 # Routing core (DashMap, bounded channels)
│   ├── network.rs                 # UDP receiver + WS server + heartbeat
│   ├── observability.rs           # Logging + Prometheus metrics + shutdown
│   └── static_files.rs            # Static file serving (stub)
├── public/                        # Static files
│   ├── index.html                 # Landing page
│   ├── chat.html                  # Chat demo
│   └── gateway_protocol.js        # JS client SDK
├── tests/
│   └── integration_test.rs        # 13 integration tests
├── examples/
│   └── stress.rs                  # Stress test benchmark
├── doc/
│   ├── gateway-protocol/          # Protocol crate (single source of truth)
│   │   ├── src/lib.rs             # 24-byte header encode/decode + tests
│   │   └── protocol.md            # Protocol specification
│   ├── rust_gateway_prd_v3.md     # PRD
│   └── multi_agent_orchestration.md
├── client_sdk/
│   ├── python/gateway_protocol.py # Python reference implementation
│   └── js/gateway_protocol.js     # JavaScript client SDK
├── .github/workflows/
│   └── build.yml                  # CI/CD: tag push + manual trigger, 5 platforms
└── scripts/
    └── build.sh                   # Cross-compile + packaging
```

## License

MIT
