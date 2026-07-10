# Rust 智能网关 (ws_gateway)

单文件、零外部业务依赖、单机高性能二进制数据总线。用于体感设备、Web 前端、AI 预测节点之间的实时二进制消息路由。

## 架构

```
                    ┌────────────────────────────────────────┐
                    │              Rust 网关 (tokio)          │
   [体感设备/UDP] ─►│  UDP 接收器 (:9999)                     │
                    │        │                                │
                    │        ▼                                │
                    │  路由核心 (DashMap 分段锁)               │
                    │        ▲                                │
                    │        │                                │
   [前端/AI/WS]  ◄─►│  WS 读写分离 (:8080) + 心跳检测          │
                    │  静态文件服务 (:8080)                     │
                    │  Metrics/日志 (:9090/metrics)            │
                    └────────────────────────────────────────┘
```

- **万物皆用户**：体感设备、网页前端、AI 节点都是平等的 Client
- **全链路二进制**：24 字节定长头部 + Payload，零拷贝路由
- **业务弱侵入**：网关只解析头部字段，不解析 Payload

## 快速开始

### 下载二进制

从 [Releases](https://github.com/ouut/ws_gateway/releases) 下载对应平台的二进制：

| 平台 | 文件 | 说明 |
|---|---|---|
| Linux x86_64 | `gateway-linux-x86_64` | |
| Linux ARM64 | `gateway-linux-aarch64` | 树莓派、ARM 服务器 |
| Windows x86_64 | `gateway-windows-x86_64.exe` | |
| macOS (Intel/M 芯片) | 见下方编译 | Release 无预编译二进制，需本地编译 |

### 运行

```bash
# Linux
chmod +x gateway-linux-x86_64
./gateway-linux-x86_64

# macOS (本地编译)
cargo build --release && ./target/release/gateway

# Windows
gateway-windows-x86_64.exe
```

**监听端口：**

| 端口 | 协议 | 用途 |
|---|---|---|
| 8080 | HTTP / WebSocket | WS 连接 + 静态文件 |
| 9999 | UDP | 体感设备数据接入 |
| 9090 | HTTP | Prometheus metrics |

### 从源码编译

**前置条件：** Rust 1.70+

```bash
git clone https://github.com/ouut/ws_gateway.git
cd ws_gateway

# Linux / macOS — 一条命令
cargo build --release && ./target/release/gateway
```

#### macOS 编译（Intel / M 芯片通用）

macOS 本地编译无需额外配置，Rust 自动检测本机架构：

```bash
# 安装 Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# 编译（自动适配 Intel x86_64 或 Apple Silicon aarch64）
cd gateway
cargo build --release
./target/release/gateway
```

#### 交叉编译（Linux 上编译 Windows / ARM）

```bash
# 安装交叉编译工具链（Debian/Ubuntu）
apt-get install gcc-mingw-w64-x86-64 gcc-aarch64-linux-gnu

# 一键编译打包 Linux x86_64 / ARM64 / Windows
./scripts/build.sh
```

> macOS 交叉编译需要 Apple SDK (osxcross)，建议直接在 macOS 机器上本地编译。`gateway-protocol` 库可在 Linux 上交叉编译到 macOS。

## 客户端接入

### WebSocket（浏览器 / AI 客户端）

```
ws://host:8080/ws?room=<ROOM>&user=<USER>
```

参数说明：
- `room` — 房间 ID，最长 6 字节 ASCII
- `user` — 用户 ID，最长 8 字节 ASCII
- 无鉴权（内网可信环境）

**示例：**

```javascript
const ws = new WebSocket('ws://localhost:8080/ws?room=ninja1&user=player_A');
ws.binaryType = 'arraybuffer';

// 接收消息
ws.onmessage = (event) => {
    const data = new Uint8Array(event.data);
    // 用 gateway_protocol.js 解析 24 字节头部 + payload
    const packet = gatewayProtocol.decode(data);
    console.log(packet.roomId, packet.payload);
};

// 发送消息
ws.send(encodedBytes);
```

### JavaScript SDK

```javascript
// 从 client_sdk/js/ 引入
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
# wire 可直接写入 WebSocket / UDP socket
```

## 协议

24 字节定长二进制头部（大端序）：

```
偏移: 0      1        2        3        4         10        18         22   24
     +------+--------+--------+--------+---------+---------+----------+------+
     |Ver(1)|PktTy(1)|TgtTy(1)|Resv(1) |RoomID(6)|UserID(8)|Seq(4,BE)|Len(2,BE)|
     +------+--------+--------+--------+---------+---------+----------+------+
```

| 字段 | 偏移 | 长度 | 说明 |
|---|---|---|---|
| Version | 0 | 1B | `0x02` |
| Packet Type | 1 | 1B | `0x01` RawMotion / `0x02` AiEvent / `0x03` SystemCmd / `0x04` Heartbeat |
| Target Type | 2 | 1B | `0x01` Broadcast / `0x02` Unicast |
| Reserved | 3 | 1B | 必须为 `0x00` |
| Room ID | 4 | 6B | ASCII 定长，不足右侧补 `\0` |
| User ID | 10 | 8B | 同上 |
| Sequence | 18 | 4B | u32 BE，发送方单调递增 |
| Length | 22 | 2B | u16 BE，Payload 字节数 |

详见 [protocol.md](doc/gateway-protocol/protocol.md)。

## 配置

网关通过环境变量配置（当前使用默认值）：

| 变量 | 默认值 | 说明 |
|---|---|---|
| `UDP_PORT` | 9999 | UDP 监听端口 |
| `WS_PORT` | 8080 | WebSocket + 静态文件端口 |
| `METRICS_PORT` | 9090 | Prometheus metrics 端口 |
| `PING_INTERVAL` | 10s | WS 心跳间隔 |
| `PONG_TIMEOUT` | 30s | WS 心跳超时 |
| `CHANNEL_CAPACITY` | 64 | 每连接发送队列容量 |

日志级别：`RUST_LOG=gateway=info`

## Metrics

访问 `http://host:9090/metrics` 查看 Prometheus 格式指标：

| 指标 | 说明 |
|---|---|
| `active_connections_total` | 活跃连接数（按 room 标签） |
| `active_rooms_total` | 活跃房间数 |
| `packets_routed_total` | 路由包数（按 Packet Type / Target Type） |
| `packets_dropped_total` | 丢弃包数（按丢弃原因） |
| `route_latency_seconds` | 路由延迟直方图 |

## 测试

```bash
# 协议 crate
cd doc/gateway-protocol && cargo test    # 10 tests

# 网关
cd gateway && cargo test                 # 10 tests

# JS SDK
node client_sdk/js/gateway_protocol.js   # self-tests
```

## 安全说明

本网关**不做鉴权和数据完整性校验**，设计用于**内网可信环境**。

- ❌ 不得直接暴露 `:8080`（WS）或 `:9999`（UDP）到公网
- ❌ Payload 损坏由业务方自行兜底，网关不校验
- ✅ 生产环境建议前端加 nginx/CDN，网关仅处理 WS 和 UDP

## 项目结构

```
ws_gateway/
├── doc/
│   ├── gateway-protocol/          # 协议 crate（单一真源）
│   │   ├── src/lib.rs             # 24 字节头部编码/解码 + 测试
│   │   └── protocol.md            # 协议规范
│   ├── rust_gateway_prd_v3.md     # PRD
│   └── multi_agent_orchestration.md
├── gateway/                       # 网关实现
│   ├── src/
│   │   ├── main.rs                # 入口
│   │   ├── routing.rs             # 路由核心
│   │   ├── network.rs             # UDP + WS 网络层
│   │   ├── observability.rs       # 日志 + metrics + 优雅关闭
│   │   └── static_files.rs        # 静态文件服务
│   └── public/                    # 静态文件根目录
├── client_sdk/
│   ├── python/gateway_protocol.py # Python 参考实现
│   └── js/gateway_protocol.js     # JavaScript 参考实现
└── scripts/
    └── build.sh                   # 交叉编译打包脚本
```

## License

MIT
