# gateway-protocol —— 协议契约（唯一真源 / Single Source of Truth）

> 版本：随 `Cargo.toml` 中的 `version` 字段递增，当前 `0.1.0`，对应线上协议 `PROTOCOL_VERSION = 0x02`。
> 状态：**已冻结**。任何字段变更必须走本文档 §5「变更流程」，禁止任何 Agent 私自修改。

---

## 1. 这是什么

本文档 + 同目录下的 `gateway-protocol` crate（Rust 代码）共同构成整个网关系统唯一的协议契约。

- 本文档是**人类可读的规范**，供你和各 Agent 理解设计意图。
- `src/lib.rs` 是**可执行的真源**：所有代码（路由核心、UDP/WS 网络层、客户端 SDK）必须直接 `use gateway_protocol::*`，禁止任何模块自己重新定义头部 struct 或手写字节偏移解析逻辑。

两者不一致时，**以代码为准**（代码有编译器和单元测试背书，文字描述没有）。

---

## 2. 头部布局（24 字节，大端序）

```text
偏移:  0      1        2        3        4         10        18         22   24
       +------+--------+--------+--------+---------+---------+----------+------+
       |Ver(1)|PktTy(1)|TgtTy(1)|Resv(1) |RoomID(6)|UserID(8)|Seq(4,BE) |Len(2,BE)|
       +------+--------+--------+--------+---------+---------+----------+------+
```

| 字段 | 偏移 | 长度 | 类型 | 说明 |
|---|---|---|---|---|
| Version | 0 | 1B | `u8` | 当前固定 `0x02`，不匹配则整包丢弃 |
| Packet Type | 1 | 1B | `PacketType` 枚举 | `0x01` RawMotion / `0x02` AiEvent / `0x03` SystemCmd / `0x04` Heartbeat |
| Target Type | 2 | 1B | `TargetType` 枚举 | `0x01` Broadcast / `0x02` Unicast |
| Reserved | 3 | 1B | — | 必须为 `0x00`，预留给未来扩展 |
| Room ID | 4 | 6B | ASCII 定长 | 不足右侧补 `\0`，超长或含非 ASCII 拒绝 |
| User ID | 10 | 8B | ASCII 定长 | 同上 |
| Sequence | 18 | 4B | `u32` BE | 发送方单调递增，网关只透传不处理 |
| Length | 22 | 2B | `u16` BE | Payload 字节数，必须与实际收到长度一致 |

**明确不包含**：CRC / 校验和字段、Token / 鉴权字段（PRD v3 第 10 章已声明不做这两项，属主动设计取舍，不是遗漏）。

---

## 3. API 一览

```rust
use gateway_protocol::{PacketHeader, PacketType, TargetType, DecodeError, EncodeError};
use bytes::Bytes;

// 编码（发送方使用，如客户端 SDK / Task D）
let header = PacketHeader::new(
    PacketType::AiEvent,
    TargetType::Broadcast,
    "ninja1",     // room_id，<=6 ASCII 字节
    "ai_bot",     // user_id，<=8 ASCII 字节
    42,           // sequence
    1,            // payload 长度
)?;
let wire_bytes: Bytes = header.encode(&[0x02]); // 直接可写入 UDP socket / WS frame

// 解码（接收方使用，如 Task A 路由核心 / Task B 网络层）
let (header, payload): (PacketHeader, Bytes) = PacketHeader::decode(incoming_bytes)?;
// payload 是零拷贝切片，不发生内存复制
```

- `PacketHeader::decode` 内部已经覆盖 PRD v3 第 9 章要求的边界情况校验：长度不足、版本不识别、Reserved 非零、Packet/Target Type 不识别、非 ASCII 字段、Length 不匹配。**下游模块不需要重复做这些校验**，只需要 `match` 处理 `DecodeError` 的各个分支，决定丢弃 + 记录哪个 metric。
- `decode` 要求输入类型是 `bytes::Bytes`（不是 `&[u8]` 或 `Vec<u8>`），因为返回的 payload 是对同一块内存的引用计数切片——这是 PRD 5.2 零拷贝要求在类型层面的强制。

---

## 4. 给各 Agent 的接入约定

| Agent / Task | 必须做的事 |
|---|---|
| Task A（路由核心） | `route()` 接口的入参类型直接用 `PacketHeader` + `Bytes`，不要自己再解析一遍字节 |
| Task B（网络 I/O） | UDP/WS 收到原始字节后，第一步就是 `PacketHeader::decode()`，校验失败按 `DecodeError` 分支丢弃并打 metric，不要自己写 `if buf.len() < 24` 这类重复逻辑 |
| Task D（客户端 SDK） | Python/JS 示例虽然不能直接 `use` 这个 crate，但必须严格按本文档 §2 的字节布局手写 struct.pack，且用本 crate 的 Rust 测试数据做交叉验证（同一组字段编码出的字节必须逐字节相同） |
| Review/QA Agent | 检查所有下游代码里有没有出现"重新定义头部字段"或"手写偏移解析"的情况——这是协议漂移的信号，一旦发现直接打回 |

---

## 5. 变更流程（协议不是不能改，但要走流程）

1. 任何 Agent 发现协议需要调整，**只能提出需求**，不能自己改 `gateway-protocol` 源码。
2. 由 Coordinator 或指定的 Protocol Agent 评估，如需修改：
   - 递增 `PROTOCOL_VERSION`（比如 `0x02` → `0x03`）
   - 更新本文档 §2 表格
   - 补充/修改 `src/lib.rs` 中的 round-trip 测试，跑 `cargo test` 全绿
3. 修改后的 crate 重新分发给所有下游 Agent，**受影响的下游会因为类型变化在 `cargo build` 阶段直接报错**，据此可以精确定位所有需要跟着改的地方，不会漏改。
4. 旧版本协议默认不兼容（`decode` 会因为 `UnsupportedVersion` 直接拒绝），如需要新旧版本共存过渡期，需另外提出灰度方案，本文档当前不覆盖。
