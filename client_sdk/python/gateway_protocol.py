#!/usr/bin/env python3
"""
gateway_protocol.py — Python 参考实现

协议真源：/home/coder/project/doc/gateway-protocol/protocol.md §2
本实现为 protocol.md §2 头部布局的手写翻译，禁止私下改动字节布局。
所有编码/解码必须与 gateway-protocol crate 的测试数据逐字节一致。

用法:
    import gateway_protocol as gp

    # 编码
    wire = gp.encode(version=0x02, pkt_type=0x01, tgt_type=0x01,
                     room_id="ninja1", user_id="player_A",
                     seq=42, payload=b'\xaa' * 2652)

    # 解码
    result = gp.decode(wire)
    # result == {"version": 2, "pkt_type": 1, "tgt_type": 1,
    #            "room_id": "ninja1", "user_id": "player_A",
    #            "seq": 42, "payload": b'\xaa' * 2652}
"""

import struct
from typing import Tuple, Dict, Optional

# ── 常量（与 gateway-protocol crate 保持一致） ──────────────────────────

PROTOCOL_VERSION: int = 0x02
HEADER_LEN: int = 24
ROOM_ID_LEN: int = 6
USER_ID_LEN: int = 8

# Packet Type 枚举
PKT_RAW_MOTION: int = 0x01
PKT_AI_EVENT: int = 0x02
PKT_SYSTEM_CMD: int = 0x03
PKT_HEARTBEAT: int = 0x04

VALID_PACKET_TYPES = frozenset(
    {PKT_RAW_MOTION, PKT_AI_EVENT, PKT_SYSTEM_CMD, PKT_HEARTBEAT}
)

# Target Type 枚举
TGT_BROADCAST: int = 0x01
TGT_UNICAST: int = 0x02

VALID_TARGET_TYPES = frozenset({TGT_BROADCAST, TGT_UNICAST})

# struct 格式：大端序，24 字节头部
# >  B     B      B      B      6s       8s       I        H
#    Ver   PktTy  TgtTy  Resv   RoomID   UserID   Seq(BE)  Len(BE)
HEADER_STRUCT = ">BBBB6s8sIH"

# ── 错误类型 ────────────────────────────────────────────────────────────


class ProtocolError(Exception):
    """网关协议相关的所有错误基类。"""
    pass


class EncodeError(ProtocolError):
    """编码时参数校验失败。"""
    pass


class DecodeError(ProtocolError):
    """解码时数据校验失败。"""
    pass


# ── 辅助函数 ────────────────────────────────────────────────────────────


def _validate_ascii_field(value: bytes, max_len: int, field_name: str) -> bytes:
    """校验并补齐 ASCII 定长字段。

    - value 必须是 bytes，每个字节必须在 0x01-0x7F 范围内
    - 长度不超过 max_len
    - 右侧补 \\x00 至 max_len
    """
    if not isinstance(value, bytes):
        raise EncodeError(f"{field_name} must be bytes, got {type(value).__name__}")
    if len(value) > max_len:
        raise EncodeError(
            f"{field_name} too long: {len(value)} bytes, max {max_len}"
        )
    for b in value:
        if b == 0 or b > 0x7F:
            raise EncodeError(
                f"{field_name} contains non-ASCII or embedded null byte: {value!r}"
            )
    # 右侧补零
    return value.ljust(max_len, b"\x00")


def _decode_ascii_field(field: bytes, field_name: str) -> str:
    """从定长 ASCII 字段中提取字符串（去除右侧补零）。

    同时校验：补零前全部是 ASCII，补零后全部是零，不允许中间嵌入零。
    """
    first_zero = field.find(b"\x00")
    if first_zero == -1:
        data = field
        padding = b""
    else:
        data = field[:first_zero]
        padding = field[first_zero:]

    # 数据部分不能包含零
    if b"\x00" in data:
        raise DecodeError(
            f"{field_name} has embedded null byte before padding"
        )
    # 数据部分必须是合法 ASCII（0x01-0x7F）
    for b in data:
        if b > 0x7F:
            raise DecodeError(
                f"{field_name} contains non-ASCII byte: {field!r}"
            )
    # 补零部分必须全部是零
    if not all(b == 0 for b in padding):
        raise DecodeError(
            f"{field_name} has non-zero bytes after padding: {field!r}"
        )

    return data.decode("ascii")


# ── 公共 API ────────────────────────────────────────────────────────────


def encode(
    version: int,
    pkt_type: int,
    tgt_type: int,
    room_id: str,
    user_id: str,
    seq: int,
    payload: bytes,
) -> bytes:
    """将协议参数 + payload 编码为完整网络字节序列（24 字节头 + payload）。

    Args:
        version: 协议版本号，固定 0x02。
        pkt_type: Packet Type，0x01-0x04。
        tgt_type: Target Type，0x01 或 0x02。
        room_id: ASCII 房间 ID，最多 6 字节。
        user_id: ASCII 用户 ID，最多 8 字节。
        seq: 序列号，u32 范围。
        payload: 负载字节。

    Returns:
        完整的网络字节序列（bytes）。

    Raises:
        EncodeError: 参数校验失败。
    """
    # 版本校验
    if version != PROTOCOL_VERSION:
        raise EncodeError(
            f"unsupported version: {version:#x}, expected {PROTOCOL_VERSION:#x}"
        )

    # 类型校验
    if pkt_type not in VALID_PACKET_TYPES:
        raise EncodeError(f"unknown packet type: {pkt_type:#x}")
    if tgt_type not in VALID_TARGET_TYPES:
        raise EncodeError(f"unknown target type: {tgt_type:#x}")

    # 长度校验
    if seq < 0 or seq > 0xFFFF_FFFF:
        raise EncodeError(f"sequence out of u32 range: {seq}")
    if len(payload) > 0xFFFF:
        raise EncodeError(f"payload too large: {len(payload)} bytes, max 65535")

    # 字符串字段
    room_bytes = room_id.encode("ascii")
    user_bytes = user_id.encode("ascii")

    padded_room = _validate_ascii_field(room_bytes, ROOM_ID_LEN, "room_id")
    padded_user = _validate_ascii_field(user_bytes, USER_ID_LEN, "user_id")

    reserved = 0  # Reserved 字段必须为 0x00

    header = struct.pack(
        HEADER_STRUCT,
        version,
        pkt_type,
        tgt_type,
        reserved,
        padded_room,
        padded_user,
        seq,
        len(payload),
    )

    return header + payload


def decode(raw_bytes: bytes) -> dict:
    """从网络字节序列解码出结构化字段 + payload。

    Args:
        raw_bytes: 收到的原始字节（至少 24 字节）。

    Returns:
        dict: {
            "version": int,
            "pkt_type": int,
            "tgt_type": int,
            "room_id": str,
            "user_id": str,
            "seq": int,
            "payload": bytes,
        }

    Raises:
        DecodeError: 数据校验失败（长度不足、版本不匹配、类型未知、非 ASCII、长度不一致）。
    """
    if len(raw_bytes) < HEADER_LEN:
        raise DecodeError(
            f"packet too short: {len(raw_bytes)} bytes, need at least {HEADER_LEN}"
        )

    # 解包 24 字节头部
    (
        version,
        pkt_type,
        tgt_type,
        reserved,
        room_field,
        user_field,
        seq,
        declared_len,
    ) = struct.unpack_from(HEADER_STRUCT, raw_bytes, 0)

    # 版本校验
    if version != PROTOCOL_VERSION:
        raise DecodeError(
            f"unsupported protocol version: {version:#x}, expected {PROTOCOL_VERSION:#x}"
        )

    # 类型校验
    if pkt_type not in VALID_PACKET_TYPES:
        raise DecodeError(f"unknown packet type: {pkt_type:#x}")
    if tgt_type not in VALID_TARGET_TYPES:
        raise DecodeError(f"unknown target type: {tgt_type:#x}")

    # 保留字段校验
    if reserved != 0:
        raise DecodeError(f"reserved field must be 0, got {reserved:#x}")

    # ASCII 字段解码
    room_id_str = _decode_ascii_field(room_field, "room_id")
    user_id_str = _decode_ascii_field(user_field, "user_id")

    # 提取 payload
    payload = raw_bytes[HEADER_LEN:]

    # 长度一致性校验
    if len(payload) != declared_len:
        raise DecodeError(
            f"length mismatch: declared {declared_len}, actual {len(payload)}"
        )

    return {
        "version": version,
        "pkt_type": pkt_type,
        "tgt_type": tgt_type,
        "room_id": room_id_str,
        "user_id": user_id_str,
        "seq": seq,
        "payload": payload,
    }


# ── 自检 / 交叉验证 ─────────────────────────────────────────────────────


def _self_test() -> bool:
    """用 Rust gateway-protocol crate 的测试用例做交叉验证。

    以下硬编码的字节序列来自 Rust crate 中对应测试的 encode 输出。
    如果 encode 输出与预期逐字节相同，说明 Python 实现与 Rust 真源一致。

    Returns:
        True 表示全部通过，否则抛出 AssertionError。
    """
    # ── 测试向量 1：对应 Rust 测试 round_trip_short_ids_are_padded_correctly ──
    #   PacketType::AiEvent (0x02), TargetType::Unicast (0x02)
    #   room_id="r1", user_id="ai", seq=0, payload=[0x02]
    expected1 = bytes(
        [
            # Header (24 bytes)
            0x02,  # version
            0x02,  # pkt_type (AiEvent)
            0x02,  # tgt_type (Unicast)
            0x00,  # reserved
            0x72, 0x31, 0x00, 0x00, 0x00, 0x00,  # room_id "r1\0\0\0\0"
            0x61, 0x69, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,  # user_id "ai\0\0\0\0\0\0"
            0x00, 0x00, 0x00, 0x00,  # seq = 0 (BE)
            0x00, 0x01,  # length = 1 (BE)
            # Payload (1 byte)
            0x02,
        ]
    )
    result1 = encode(
        version=0x02,
        pkt_type=PKT_AI_EVENT,
        tgt_type=TGT_UNICAST,
        room_id="r1",
        user_id="ai",
        seq=0,
        payload=b"\x02",
    )
    assert result1 == expected1, (
        f"Test vector 1 mismatch:\n"
        f"  expected: {expected1.hex()}\n"
        f"  got:      {result1.hex()}"
    )

    # Round-trip decode on the same vector
    decoded1 = decode(expected1)
    assert decoded1["version"] == 2
    assert decoded1["pkt_type"] == PKT_AI_EVENT
    assert decoded1["tgt_type"] == TGT_UNICAST
    assert decoded1["room_id"] == "r1"
    assert decoded1["user_id"] == "ai"
    assert decoded1["seq"] == 0
    assert decoded1["payload"] == b"\x02"

    # ── 测试向量 2：对应 Rust 测试 sequence_and_length_roundtrip_exact ──
    #   PacketType::SystemCmd (0x03), TargetType::Unicast (0x02)
    #   room_id="roomid", user_id="useridid", seq=0xFFFFFFFE, payload=[1,2,3,4,5]
    expected2 = bytes(
        [
            # Header (24 bytes)
            0x02,  # version
            0x03,  # pkt_type (SystemCmd)
            0x02,  # tgt_type (Unicast)
            0x00,  # reserved
            0x72, 0x6F, 0x6F, 0x6D, 0x69, 0x64,  # room_id "roomid"
            0x75, 0x73, 0x65, 0x72, 0x69, 0x64, 0x69, 0x64,  # user_id "useridid"
            0xFF, 0xFF, 0xFF, 0xFE,  # seq = 0xFFFFFFFE (BE)
            0x00, 0x05,  # length = 5 (BE)
            # Payload (5 bytes)
            0x01, 0x02, 0x03, 0x04, 0x05,
        ]
    )
    result2 = encode(
        version=0x02,
        pkt_type=PKT_SYSTEM_CMD,
        tgt_type=TGT_UNICAST,
        room_id="roomid",
        user_id="useridid",
        seq=0xFFFFFFFE,
        payload=b"\x01\x02\x03\x04\x05",
    )
    assert result2 == expected2, (
        f"Test vector 2 mismatch:\n"
        f"  expected: {expected2.hex()}\n"
        f"  got:      {result2.hex()}"
    )

    # Round-trip decode
    decoded2 = decode(expected2)
    assert decoded2["version"] == 2
    assert decoded2["pkt_type"] == PKT_SYSTEM_CMD
    assert decoded2["tgt_type"] == TGT_UNICAST
    assert decoded2["room_id"] == "roomid"
    assert decoded2["user_id"] == "useridid"
    assert decoded2["seq"] == 0xFFFFFFFE
    assert decoded2["payload"] == b"\x01\x02\x03\x04\x05"

    # ── 测试向量 3：对应 Rust 测试 round_trip_basic（仅验证头部） ──
    #   PacketType::RawMotion (0x01), TargetType::Broadcast (0x01)
    #   room_id="ninja1", user_id="player_A", seq=42, payload_len=0
    # 这里用空 payload 来验证头部字节，避免硬编码 2652 字节
    expected3_header = bytes(
        [
            0x02,  # version
            0x01,  # pkt_type (RawMotion)
            0x01,  # tgt_type (Broadcast)
            0x00,  # reserved
            0x6E, 0x69, 0x6E, 0x6A, 0x61, 0x31,  # room_id "ninja1"
            0x70, 0x6C, 0x61, 0x79, 0x65, 0x72, 0x5F, 0x41,  # user_id "player_A"
            0x00, 0x00, 0x00, 0x2A,  # seq = 42 (BE)
            0x00, 0x00,  # length = 0 (BE)
        ]
    )
    result3 = encode(
        version=0x02,
        pkt_type=PKT_RAW_MOTION,
        tgt_type=TGT_BROADCAST,
        room_id="ninja1",
        user_id="player_A",
        seq=42,
        payload=b"",
    )
    assert result3 == expected3_header, (
        f"Test vector 3 mismatch:\n"
        f"  expected: {expected3_header.hex()}\n"
        f"  got:      {result3.hex()}"
    )

    # ── 错误路径测试 ──

    # 版本不匹配
    try:
        decode(
            bytes(
                [
                    0x99,  # wrong version
                    0x04,
                    0x01,
                    0x00,
                ]
                + [0x00] * 20
            )
        )
        assert False, "should have raised DecodeError for wrong version"
    except DecodeError:
        pass

    # 长度不足
    try:
        decode(b"\x00" * 10)
        assert False, "should have raised DecodeError for too short"
    except DecodeError:
        pass

    # 未知包类型
    try:
        decode(
            bytes(
                [
                    0x02,
                    0xFF,  # unknown pkt_type
                    0x01,
                    0x00,
                ]
                + [0x00] * 20
            )
        )
        assert False, "should have raised DecodeError for unknown pkt_type"
    except DecodeError:
        pass

    # 未知目标类型
    try:
        decode(
            bytes(
                [
                    0x02,
                    0x01,
                    0xFF,  # unknown tgt_type
                    0x00,
                ]
                + [0x00] * 20
            )
        )
        assert False, "should have raised DecodeError for unknown tgt_type"
    except DecodeError:
        pass

    # room_id 过长
    try:
        encode(0x02, PKT_HEARTBEAT, TGT_BROADCAST, "toolong123", "u1", 0, b"")
        assert False, "should have raised EncodeError for too-long room_id"
    except EncodeError:
        pass

    # room_id 含非 ASCII
    try:
        encode(0x02, PKT_HEARTBEAT, TGT_BROADCAST, "房间", "u1", 0, b"")
        assert False, "should have raised EncodeError for non-ASCII room_id"
    except (EncodeError, UnicodeEncodeError):
        # UnicodeEncodeError from .encode("ascii") is also acceptable
        pass

    print("gateway_protocol.py: all self-tests PASSED")
    return True


# ── 命令行入口 ───────────────────────────────────────────────────────────

if __name__ == "__main__":
    _self_test()
