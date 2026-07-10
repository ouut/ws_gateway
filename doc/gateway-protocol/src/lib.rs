//! gateway-protocol
//!
//! 单一真源（single source of truth）：Rust 智能网关的二进制协议头定义。
//! 对应 PRD v3 第 4 章。所有下游模块（路由核心、UDP/WS 网络层、客户端 SDK）
//! 必须依赖本 crate 提供的类型和函数，禁止自行重新实现字节偏移解析逻辑。
//!
//! ## 头部布局（24 字节，大端序）
//!
//! ```text
//! 偏移:  0      1        2        3        4         10        18         22   24
//!        +------+--------+--------+--------+---------+---------+----------+------+
//!        |Ver(1)|PktTy(1)|TgtTy(1)|Resv(1) |RoomID(6)|UserID(8)|Seq(4,BE) |Len(2,BE)|
//!        +------+--------+--------+--------+---------+---------+----------+------+
//! ```
//!
//! - RoomID / UserID：ASCII 定长字符串，右侧补 `\0`。**不允许非 ASCII 字节**。
//! - 不含 CRC / Token 字段（PRD v3 明确不做完整性校验与鉴权，见第 10 章）。

use bytes::{BufMut, Bytes, BytesMut};
use std::fmt;

/// 当前协议版本号。协议升级时递增，并同步更新所有下游 Agent。
pub const PROTOCOL_VERSION: u8 = 0x02;

/// 头部总长度（字节）。
pub const HEADER_LEN: usize = 24;

/// Room ID 定长字节数。
pub const ROOM_ID_LEN: usize = 6;

/// User ID 定长字节数。
pub const USER_ID_LEN: usize = 8;

/// Packet Type：Payload 携带的业务数据类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PacketType {
    /// 0x01 原始体感骨骼数据，固定 2652 字节 ARKit 浮点数组
    RawMotion,
    /// 0x02 AI 预测出的事件/动作，[1 Byte Action ID]
    AiEvent,
    /// 0x03 系统控制指令，格式由业务方自定义
    SystemCmd,
    /// 0x04 应用层心跳（UDP 场景使用），Payload 为空
    Heartbeat,
}

impl PacketType {
    pub fn as_u8(self) -> u8 {
        match self {
            PacketType::RawMotion => 0x01,
            PacketType::AiEvent => 0x02,
            PacketType::SystemCmd => 0x03,
            PacketType::Heartbeat => 0x04,
        }
    }

    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0x01 => Some(PacketType::RawMotion),
            0x02 => Some(PacketType::AiEvent),
            0x03 => Some(PacketType::SystemCmd),
            0x04 => Some(PacketType::Heartbeat),
            _ => None,
        }
    }
}

/// Target Type：寻址策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TargetType {
    /// 0x01 广播给房间内所有连接
    Broadcast,
    /// 0x02 精准单发给房间内特定用户
    Unicast,
}

impl TargetType {
    pub fn as_u8(self) -> u8 {
        match self {
            TargetType::Broadcast => 0x01,
            TargetType::Unicast => 0x02,
        }
    }

    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0x01 => Some(TargetType::Broadcast),
            0x02 => Some(TargetType::Unicast),
            _ => None,
        }
    }
}

/// 24 字节协议头，解析后的结构化表示。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PacketHeader {
    pub version: u8,
    pub packet_type: PacketType,
    pub target_type: TargetType,
    pub room_id: [u8; ROOM_ID_LEN],
    pub user_id: [u8; USER_ID_LEN],
    pub sequence: u32,
    pub length: u16,
}

/// 组装头部时可能发生的错误。
#[derive(Debug, PartialEq, Eq)]
pub enum EncodeError {
    /// room_id 超过 6 字节，或包含非 ASCII 字符
    InvalidRoomId,
    /// user_id 超过 8 字节，或包含非 ASCII 字符
    InvalidUserId,
    /// payload 长度超过 u16 最大值
    PayloadTooLarge,
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EncodeError::InvalidRoomId => write!(f, "room_id must be <= 6 ASCII bytes"),
            EncodeError::InvalidUserId => write!(f, "user_id must be <= 8 ASCII bytes"),
            EncodeError::PayloadTooLarge => write!(f, "payload length exceeds u16::MAX"),
        }
    }
}
impl std::error::Error for EncodeError {}

/// 解析头部时可能发生的错误。对应 PRD v3 第 9 章边界情况清单。
#[derive(Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// 总长度 < 24 字节
    TooShort,
    /// Version 字段不匹配当前协议版本
    UnsupportedVersion(u8),
    /// Reserved 字段非零（必须为 0x00，PRD v3 第 4 章）
    NonZeroReserved(u8),
    /// 未知的 Packet Type
    UnknownPacketType(u8),
    /// 未知的 Target Type
    UnknownTargetType(u8),
    /// room_id 字段包含非 ASCII 字节
    NonAsciiRoomId,
    /// user_id 字段包含非 ASCII 字节
    NonAsciiUserId,
    /// Length 字段与实际收到的 payload 字节数不一致
    LengthMismatch { declared: u16, actual: usize },
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::TooShort => write!(f, "packet shorter than {HEADER_LEN}-byte header"),
            DecodeError::UnsupportedVersion(v) => write!(f, "unsupported protocol version: {v:#x}"),
            DecodeError::NonZeroReserved(v) => write!(f, "reserved field must be 0x00, got: {v:#x}"),
            DecodeError::UnknownPacketType(v) => write!(f, "unknown packet type: {v:#x}"),
            DecodeError::UnknownTargetType(v) => write!(f, "unknown target type: {v:#x}"),
            DecodeError::NonAsciiRoomId => write!(f, "room_id contains non-ASCII bytes"),
            DecodeError::NonAsciiUserId => write!(f, "user_id contains non-ASCII bytes"),
            DecodeError::LengthMismatch { declared, actual } => write!(
                f,
                "declared payload length {declared} does not match actual {actual}"
            ),
        }
    }
}
impl std::error::Error for DecodeError {}

impl PacketHeader {
    /// 构造一个新的协议头。room_id / user_id 传入普通字符串，内部负责
    /// ASCII 校验、长度校验与右侧补零。
    pub fn new(
        packet_type: PacketType,
        target_type: TargetType,
        room_id: &str,
        user_id: &str,
        sequence: u32,
        payload_len: usize,
    ) -> Result<Self, EncodeError> {
        let room_id = pack_ascii_field::<ROOM_ID_LEN>(room_id).ok_or(EncodeError::InvalidRoomId)?;
        let user_id = pack_ascii_field::<USER_ID_LEN>(user_id).ok_or(EncodeError::InvalidUserId)?;
        let length: u16 = payload_len
            .try_into()
            .map_err(|_| EncodeError::PayloadTooLarge)?;

        Ok(PacketHeader {
            version: PROTOCOL_VERSION,
            packet_type,
            target_type,
            room_id,
            user_id,
            sequence,
            length,
        })
    }

    /// room_id 去除补零后的字符串视图。
    pub fn room_id_str(&self) -> &str {
        ascii_field_str(&self.room_id)
    }

    /// user_id 去除补零后的字符串视图。
    pub fn user_id_str(&self) -> &str {
        ascii_field_str(&self.user_id)
    }

    /// 将头部序列化为 24 字节。配合 payload 一起写入 socket。
    pub fn encode_header(&self) -> [u8; HEADER_LEN] {
        let mut buf = [0u8; HEADER_LEN];
        buf[0] = self.version;
        buf[1] = self.packet_type.as_u8();
        buf[2] = self.target_type.as_u8();
        buf[3] = 0; // Reserved
        buf[4..10].copy_from_slice(&self.room_id);
        buf[10..18].copy_from_slice(&self.user_id);
        buf[18..22].copy_from_slice(&self.sequence.to_be_bytes());
        buf[22..24].copy_from_slice(&self.length.to_be_bytes());
        buf
    }

    /// 将头部 + payload 编码为一个完整的 `Bytes`，可直接写入 UDP/WS。
    pub fn encode(&self, payload: &[u8]) -> Bytes {
        let mut buf = BytesMut::with_capacity(HEADER_LEN + payload.len());
        buf.put_slice(&self.encode_header());
        buf.put_slice(payload);
        buf.freeze()
    }

    /// 从原始字节零拷贝解析出头部 + payload。
    ///
    /// 输入必须是 `bytes::Bytes`（引用计数共享缓冲区），返回的 payload
    /// 是对同一块内存的切片视图，不发生内存拷贝，符合 PRD 5.2 的零拷贝要求。
    ///
    /// 会做以下校验（对应 PRD v3 第 9 章边界情况）：
    /// - 总长度 >= 24 字节
    /// - Version 匹配
    /// - Reserved 字段必须为 0x00
    /// - Packet Type / Target Type 合法
    /// - room_id / user_id 为合法 ASCII
    /// - Length 字段与实际 payload 字节数一致
    ///
    /// **不做**：CRC / 完整性校验、鉴权（PRD v3 明确不做，见第 10 章）。
    pub fn decode(buf: Bytes) -> Result<(PacketHeader, Bytes), DecodeError> {
        if buf.len() < HEADER_LEN {
            return Err(DecodeError::TooShort);
        }

        let version = buf[0];
        if version != PROTOCOL_VERSION {
            return Err(DecodeError::UnsupportedVersion(version));
        }

        // Reserved 字段必须为 0x00（PRD v3 第 4 章明确要求）。
        if buf[3] != 0x00 {
            return Err(DecodeError::NonZeroReserved(buf[3]));
        }

        let packet_type =
            PacketType::from_u8(buf[1]).ok_or(DecodeError::UnknownPacketType(buf[1]))?;
        let target_type =
            TargetType::from_u8(buf[2]).ok_or(DecodeError::UnknownTargetType(buf[2]))?;

        let mut room_id = [0u8; ROOM_ID_LEN];
        room_id.copy_from_slice(&buf[4..10]);
        if !is_valid_ascii_field(&room_id) {
            return Err(DecodeError::NonAsciiRoomId);
        }

        let mut user_id = [0u8; USER_ID_LEN];
        user_id.copy_from_slice(&buf[10..18]);
        if !is_valid_ascii_field(&user_id) {
            return Err(DecodeError::NonAsciiUserId);
        }

        let sequence = u32::from_be_bytes(buf[18..22].try_into().unwrap());
        let length = u16::from_be_bytes(buf[22..24].try_into().unwrap());

        let actual_payload_len = buf.len() - HEADER_LEN;
        if actual_payload_len != length as usize {
            return Err(DecodeError::LengthMismatch {
                declared: length,
                actual: actual_payload_len,
            });
        }

        let header = PacketHeader {
            version,
            packet_type,
            target_type,
            room_id,
            user_id,
            sequence,
            length,
        };

        // 零拷贝：slice 只是增加引用计数，不复制底层内存
        let payload = buf.slice(HEADER_LEN..);
        Ok((header, payload))
    }
}

/// 将一个普通字符串打包为定长 ASCII 字节数组（右侧补 `\0`）。
/// 超长或含非 ASCII 字符时返回 None。
fn pack_ascii_field<const N: usize>(s: &str) -> Option<[u8; N]> {
    if !s.is_ascii() || s.len() > N {
        return None;
    }
    let mut buf = [0u8; N];
    buf[..s.len()].copy_from_slice(s.as_bytes());
    Some(buf)
}

/// 校验定长字段：必须是 ASCII，且补零部分必须全部是 `\0`（不允许中间嵌入 \0 后又出现非零字节）。
fn is_valid_ascii_field(field: &[u8]) -> bool {
    let first_zero = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    field[..first_zero].iter().all(|&b| b.is_ascii() && b != 0) 
        && field[first_zero..].iter().all(|&b| b == 0)
}

fn ascii_field_str(field: &[u8]) -> &str {
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    // 前面已经在 decode/new 阶段校验过是合法 ASCII，这里可以安全 unwrap
    std::str::from_utf8(&field[..end]).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_basic() {
        let payload = vec![0xAAu8; 2652]; // 模拟 RAW_MOTION payload
        let header = PacketHeader::new(
            PacketType::RawMotion,
            TargetType::Broadcast,
            "ninja1",
            "player_A",
            42,
            payload.len(),
        )
        .unwrap();

        let encoded = header.encode(&payload);
        let (decoded_header, decoded_payload) = PacketHeader::decode(encoded).unwrap();

        assert_eq!(decoded_header, header);
        assert_eq!(decoded_payload.as_ref(), payload.as_slice());
        assert_eq!(decoded_header.room_id_str(), "ninja1");
        assert_eq!(decoded_header.user_id_str(), "player_A");
    }

    #[test]
    fn round_trip_short_ids_are_padded_correctly() {
        let header = PacketHeader::new(
            PacketType::AiEvent,
            TargetType::Unicast,
            "r1",
            "ai",
            0,
            1,
        )
        .unwrap();
        let encoded = header.encode(&[0x02]);
        let (decoded, payload) = PacketHeader::decode(encoded).unwrap();
        assert_eq!(decoded.room_id_str(), "r1");
        assert_eq!(decoded.user_id_str(), "ai");
        assert_eq!(payload.as_ref(), &[0x02]);
    }

    #[test]
    fn reject_too_short_packet() {
        let buf = Bytes::from_static(&[0u8; 10]);
        assert_eq!(PacketHeader::decode(buf), Err(DecodeError::TooShort));
    }

    #[test]
    fn reject_unsupported_version() {
        let mut raw = [0u8; HEADER_LEN];
        raw[0] = 0x99; // wrong version
        raw[1] = PacketType::Heartbeat.as_u8();
        raw[2] = TargetType::Broadcast.as_u8();
        let buf = Bytes::copy_from_slice(&raw);
        assert_eq!(
            PacketHeader::decode(buf),
            Err(DecodeError::UnsupportedVersion(0x99))
        );
    }

    #[test]
    fn reject_unknown_packet_type() {
        let mut raw = [0u8; HEADER_LEN];
        raw[0] = PROTOCOL_VERSION;
        raw[1] = 0xFF; // unknown packet type
        raw[2] = TargetType::Broadcast.as_u8();
        let buf = Bytes::copy_from_slice(&raw);
        assert_eq!(
            PacketHeader::decode(buf),
            Err(DecodeError::UnknownPacketType(0xFF))
        );
    }

    #[test]
    fn reject_non_zero_reserved() {
        let mut raw = [0u8; HEADER_LEN];
        raw[0] = PROTOCOL_VERSION;
        raw[1] = PacketType::Heartbeat.as_u8();
        raw[2] = TargetType::Broadcast.as_u8();
        raw[3] = 0xFF; // reserved 非零，应被拒绝
        let buf = Bytes::copy_from_slice(&raw);
        assert_eq!(
            PacketHeader::decode(buf),
            Err(DecodeError::NonZeroReserved(0xFF))
        );
    }

    #[test]
    fn reject_length_mismatch() {
        let header = PacketHeader::new(
            PacketType::Heartbeat,
            TargetType::Broadcast,
            "r1",
            "u1",
            0,
            0,
        )
        .unwrap();
        let mut raw = header.encode_header().to_vec();
        raw.extend_from_slice(&[1, 2, 3]); // 实际带了 3 字节 payload，但 Length 声明是 0
        let buf = Bytes::from(raw);
        assert_eq!(
            PacketHeader::decode(buf),
            Err(DecodeError::LengthMismatch {
                declared: 0,
                actual: 3
            })
        );
    }

    #[test]
    fn reject_room_id_too_long() {
        let err = PacketHeader::new(
            PacketType::Heartbeat,
            TargetType::Broadcast,
            "toolong123",
            "u1",
            0,
            0,
        )
        .unwrap_err();
        assert_eq!(err, EncodeError::InvalidRoomId);
    }

    #[test]
    fn reject_non_ascii_room_id() {
        let err = PacketHeader::new(
            PacketType::Heartbeat,
            TargetType::Broadcast,
            "房间",
            "u1",
            0,
            0,
        )
        .unwrap_err();
        assert_eq!(err, EncodeError::InvalidRoomId);
    }

    #[test]
    fn sequence_and_length_roundtrip_exact() {
        let header = PacketHeader::new(
            PacketType::SystemCmd,
            TargetType::Unicast,
            "roomid",
            "useridid",
            u32::MAX - 1,
            5,
        )
        .unwrap();
        let encoded = header.encode(&[1, 2, 3, 4, 5]);
        let (decoded, _) = PacketHeader::decode(encoded).unwrap();
        assert_eq!(decoded.sequence, u32::MAX - 1);
        assert_eq!(decoded.length, 5);
    }
}
