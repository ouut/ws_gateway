/**
 * gateway_protocol.js — JavaScript / TypeScript 参考实现
 *
 * 协议真源：/home/coder/project/doc/gateway-protocol/protocol.md §2
 * 本实现为 protocol.md §2 头部布局的手写翻译，禁止私下改动字节布局。
 * 所有编码/解码必须与 gateway-protocol crate 的测试数据逐字节一致。
 *
 * 用法（ESM / CommonJS / 浏览器全局均支持，单文件无外部依赖）:
 *
 *   // Node.js / bundler
 *   const gp = require('./gateway_protocol.js');
 *   // 或
 *   import * as gp from './gateway_protocol.js';
 *
 *   const wire = gp.encode({
 *       version: 0x02,
 *       pktType: gp.PKT_AI_EVENT,
 *       tgtType: gp.TGT_BROADCAST,
 *       roomId: 'ninja1',
 *       userId: 'player_A',
 *       seq: 42,
 *       payload: new Uint8Array([0x02]),
 *   });
 *
 *   const obj = gp.decode(wire);
 *   console.log(obj.roomId); // 'ninja1'
 */

'use strict';

// ── 常量（与 gateway-protocol crate 保持一致） ──────────────────────────

const PROTOCOL_VERSION = 0x02;
const HEADER_LEN = 24;
const ROOM_ID_LEN = 6;
const USER_ID_LEN = 8;

// Packet Type 枚举
const PKT_RAW_MOTION = 0x01;
const PKT_AI_EVENT = 0x02;
const PKT_SYSTEM_CMD = 0x03;
const PKT_HEARTBEAT = 0x04;

const VALID_PACKET_TYPES = new Set([
    PKT_RAW_MOTION, PKT_AI_EVENT, PKT_SYSTEM_CMD, PKT_HEARTBEAT,
]);

// Target Type 枚举
const TGT_BROADCAST = 0x01;
const TGT_UNICAST = 0x02;

const VALID_TARGET_TYPES = new Set([TGT_BROADCAST, TGT_UNICAST]);

// ── 错误类型 ────────────────────────────────────────────────────────────

/** 网关协议相关的所有错误基类。 */
class ProtocolError extends Error {
    constructor(message) {
        super(message);
        this.name = 'ProtocolError';
    }
}

/** 编码时参数校验失败。 */
class EncodeError extends ProtocolError {
    constructor(message) {
        super(message);
        this.name = 'EncodeError';
    }
}

/** 解码时数据校验失败。 */
class DecodeError extends ProtocolError {
    constructor(message) {
        super(message);
        this.name = 'DecodeError';
    }
}

// ── 辅助函数 ────────────────────────────────────────────────────────────

/**
 * 校验并补齐 ASCII 定长字段。
 *
 * @param {string} value - 输入字符串
 * @param {number} maxLen - 最大字节数
 * @param {string} fieldName - 字段名（用于错误消息）
 * @returns {Uint8Array} 补齐后的定长字节数组（右侧补零）
 * @throws {EncodeError}
 */
function packAsciiField(value, maxLen, fieldName) {
    if (typeof value !== 'string') {
        throw new EncodeError(fieldName + ' must be a string, got ' + typeof value);
    }
    if (value.length > maxLen) {
        throw new EncodeError(
            fieldName + ' too long: ' + value.length + ' chars, max ' + maxLen
        );
    }
    // 检查是否全部为 ASCII（0x01-0x7F），不允许嵌入 null
    for (let i = 0; i < value.length; i++) {
        const code = value.charCodeAt(i);
        if (code === 0 || code > 0x7F) {
            throw new EncodeError(
                fieldName + ' contains non-ASCII or null char at index ' + i + ': ' + value
            );
        }
    }
    // 右侧补零
    const out = new Uint8Array(maxLen);
    for (let i = 0; i < value.length; i++) {
        out[i] = value.charCodeAt(i);
    }
    // 其余字节保持为 0（Uint8Array 默认值）
    return out;
}

/**
 * 从定长 ASCII 字段中提取字符串（去除右侧补零）并校验。
 *
 * @param {Uint8Array} field - 定长字节数组
 * @param {string} fieldName - 字段名（用于错误消息）
 * @returns {string}
 * @throws {DecodeError}
 */
function unpackAsciiField(field, fieldName) {
    let firstZero = -1;
    for (let i = 0; i < field.length; i++) {
        if (field[i] === 0) {
            firstZero = i;
            break;
        }
    }

    const dataEnd = firstZero === -1 ? field.length : firstZero;

    // 校验数据部分
    for (let i = 0; i < dataEnd; i++) {
        const b = field[i];
        if (b > 0x7F) {
            throw new DecodeError(
                fieldName + ' contains non-ASCII byte 0x' + b.toString(16) + ' at index ' + i
            );
        }
        if (b === 0) {
            throw new DecodeError(
                fieldName + ' has embedded null byte at index ' + i
            );
        }
    }

    // 校验补零部分
    for (let i = dataEnd; i < field.length; i++) {
        if (field[i] !== 0) {
            throw new DecodeError(
                fieldName + ' has non-zero byte 0x' + field[i].toString(16) + ' after padding at index ' + i
            );
        }
    }

    // 将有效部分解码为字符串
    let str = '';
    for (let i = 0; i < dataEnd; i++) {
        str += String.fromCharCode(field[i]);
    }
    return str;
}

// ── 公共 API ────────────────────────────────────────────────────────────

/**
 * 将协议参数 + payload 编码为完整网络字节序列（24 字节头 + payload）。
 *
 * @param {Object} opts
 * @param {number} opts.version - 协议版本号，固定 0x02。
 * @param {number} opts.pktType - Packet Type，0x01-0x04。
 * @param {number} opts.tgtType - Target Type，0x01 或 0x02。
 * @param {string} opts.roomId - ASCII 房间 ID，最多 6 字节。
 * @param {string} opts.userId - ASCII 用户 ID，最多 8 字节。
 * @param {number} opts.seq - 序列号，u32 范围（0 到 0xFFFFFFFF）。
 * @param {Uint8Array} opts.payload - 负载字节。
 * @returns {Uint8Array} 完整的网络字节序列。
 * @throws {EncodeError}
 */
function encode(opts) {
    const version = opts.version;
    const pktType = opts.pktType;
    const tgtType = opts.tgtType;
    const roomId = opts.roomId;
    const userId = opts.userId;
    const seq = opts.seq;
    const payload = opts.payload;

    // 版本校验
    if (version !== PROTOCOL_VERSION) {
        throw new EncodeError(
            'unsupported version: 0x' + version.toString(16) +
            ', expected 0x' + PROTOCOL_VERSION.toString(16)
        );
    }

    // 类型校验
    if (!VALID_PACKET_TYPES.has(pktType)) {
        throw new EncodeError('unknown packet type: 0x' + pktType.toString(16));
    }
    if (!VALID_TARGET_TYPES.has(tgtType)) {
        throw new EncodeError('unknown target type: 0x' + tgtType.toString(16));
    }

    // 序列号范围
    if (!Number.isInteger(seq) || seq < 0 || seq > 0xFFFFFFFF) {
        throw new EncodeError('sequence out of u32 range: ' + seq);
    }

    // payload 长度
    if (payload.length > 0xFFFF) {
        throw new EncodeError('payload too large: ' + payload.length + ' bytes, max 65535');
    }

    // 字符串字段
    const roomField = packAsciiField(roomId, ROOM_ID_LEN, 'roomId');
    const userField = packAsciiField(userId, USER_ID_LEN, 'userId');

    // 组装
    const totalLen = HEADER_LEN + payload.length;
    const buf = new ArrayBuffer(totalLen);
    const view = new DataView(buf);
    const out = new Uint8Array(buf);

    view.setUint8(0, version);
    view.setUint8(1, pktType);
    view.setUint8(2, tgtType);
    view.setUint8(3, 0); // reserved

    out.set(roomField, 4);
    out.set(userField, 10);

    view.setUint32(18, seq, false); // big-endian
    view.setUint16(22, payload.length, false); // big-endian

    out.set(payload, HEADER_LEN);

    // Defensive: verify first byte is version (0x02).
    // Some browser WebSocket implementations mishandle TypedArray → Binary frame.
    if (out[0] !== PROTOCOL_VERSION) {
        throw new EncodeError(
            'internal: first byte must be 0x' + PROTOCOL_VERSION.toString(16) +
            ', got 0x' + out[0].toString(16)
        );
    }

    return out;
}

/**
 * 从网络字节序列解码出结构化字段 + payload。
 *
 * @param {Uint8Array|ArrayBuffer|Buffer} rawBytes - 收到的原始字节（至少 24 字节）。
 * @returns {Object} {
 *     version: number,
 *     pktType: number,
 *     tgtType: number,
 *     roomId: string,
 *     userId: string,
 *     seq: number,
 *     payload: Uint8Array,
 * }
 * @throws {DecodeError}
 */
function decode(rawBytes) {
    // 统一转换为 Uint8Array
    let buf;
    if (rawBytes instanceof Uint8Array) {
        buf = rawBytes;
    } else if (rawBytes instanceof ArrayBuffer) {
        buf = new Uint8Array(rawBytes);
    } else if (rawBytes.buffer !== undefined) {
        // Node.js Buffer / TypedArray
        buf = new Uint8Array(rawBytes.buffer, rawBytes.byteOffset, rawBytes.byteLength);
    } else {
        throw new DecodeError('decode requires Uint8Array, ArrayBuffer, or Buffer-like');
    }

    if (buf.length < HEADER_LEN) {
        throw new DecodeError(
            'packet too short: ' + buf.length + ' bytes, need at least ' + HEADER_LEN
        );
    }

    const view = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);

    const version = view.getUint8(0);
    const pktType = view.getUint8(1);
    const tgtType = view.getUint8(2);
    const reserved = view.getUint8(3);

    // 版本校验
    if (version !== PROTOCOL_VERSION) {
        throw new DecodeError(
            'unsupported protocol version: 0x' + version.toString(16) +
            ', expected 0x' + PROTOCOL_VERSION.toString(16)
        );
    }

    // 类型校验
    if (!VALID_PACKET_TYPES.has(pktType)) {
        throw new DecodeError('unknown packet type: 0x' + pktType.toString(16));
    }
    if (!VALID_TARGET_TYPES.has(tgtType)) {
        throw new DecodeError('unknown target type: 0x' + tgtType.toString(16));
    }

    // 保留字段
    if (reserved !== 0) {
        throw new DecodeError('reserved field must be 0, got 0x' + reserved.toString(16));
    }

    // ASCII 字段
    const roomField = buf.slice(4, 10);
    const userField = buf.slice(10, 18);
    const roomId = unpackAsciiField(roomField, 'roomId');
    const userId = unpackAsciiField(userField, 'userId');

    const seq = view.getUint32(18, false); // big-endian
    const declaredLen = view.getUint16(22, false); // big-endian

    // payload
    const payload = buf.slice(HEADER_LEN);

    // 长度一致性
    if (payload.length !== declaredLen) {
        throw new DecodeError(
            'length mismatch: declared ' + declaredLen + ', actual ' + payload.length
        );
    }

    return {
        version: version,
        pktType: pktType,
        tgtType: tgtType,
        roomId: roomId,
        userId: userId,
        seq: seq,
        payload: payload,
    };
}

// ── 自检 / 交叉验证 ─────────────────────────────────────────────────────

/**
 * 用 Rust gateway-protocol crate 的测试用例做交叉验证。
 *
 * 以下硬编码的字节序列来自 Rust crate 中对应测试的 encode 输出。
 * 如果 encode 输出与预期逐字节相同，说明 JS 实现与 Rust 真源一致。
 *
 * @returns {boolean} true 表示全部通过。
 * @throws {Error} 任何断言失败都会抛出。
 */
function selfTest() {
    // ── 测试向量 1：对应 Rust 测试 round_trip_short_ids_are_padded_correctly ──
    //   PacketType::AiEvent (0x02), TargetType::Unicast (0x02)
    //   room_id="r1", user_id="ai", seq=0, payload=[0x02]
    var expected1 = new Uint8Array([
        // Header (24 bytes)
        0x02, // version
        0x02, // pktType (AiEvent)
        0x02, // tgtType (Unicast)
        0x00, // reserved
        0x72, 0x31, 0x00, 0x00, 0x00, 0x00, // roomId "r1\0\0\0\0"
        0x61, 0x69, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // userId "ai\0\0\0\0\0\0"
        0x00, 0x00, 0x00, 0x00, // seq = 0 (BE)
        0x00, 0x01, // length = 1 (BE)
        // Payload (1 byte)
        0x02,
    ]);

    var result1 = encode({
        version: 0x02,
        pktType: PKT_AI_EVENT,
        tgtType: TGT_UNICAST,
        roomId: 'r1',
        userId: 'ai',
        seq: 0,
        payload: new Uint8Array([0x02]),
    });

    if (!arraysEqual(result1, expected1)) {
        throw new Error(
            'Test vector 1 mismatch:\n' +
            '  expected: ' + toHex(expected1) + '\n' +
            '  got:      ' + toHex(result1)
        );
    }

    // Round-trip decode on the same vector
    var decoded1 = decode(expected1);
    if (decoded1.version !== 2) throw new Error('TV1 version mismatch');
    if (decoded1.pktType !== PKT_AI_EVENT) throw new Error('TV1 pktType mismatch');
    if (decoded1.tgtType !== TGT_UNICAST) throw new Error('TV1 tgtType mismatch');
    if (decoded1.roomId !== 'r1') throw new Error('TV1 roomId mismatch');
    if (decoded1.userId !== 'ai') throw new Error('TV1 userId mismatch');
    if (decoded1.seq !== 0) throw new Error('TV1 seq mismatch');
    if (!arraysEqual(decoded1.payload, new Uint8Array([0x02]))) throw new Error('TV1 payload mismatch');

    // ── 测试向量 2：对应 Rust 测试 sequence_and_length_roundtrip_exact ──
    //   PacketType::SystemCmd (0x03), TargetType::Unicast (0x02)
    //   room_id="roomid", user_id="useridid", seq=0xFFFFFFFE, payload=[1,2,3,4,5]
    var expected2 = new Uint8Array([
        // Header (24 bytes)
        0x02, // version
        0x03, // pktType (SystemCmd)
        0x02, // tgtType (Unicast)
        0x00, // reserved
        0x72, 0x6F, 0x6F, 0x6D, 0x69, 0x64, // roomId "roomid"
        0x75, 0x73, 0x65, 0x72, 0x69, 0x64, 0x69, 0x64, // userId "useridid"
        0xFF, 0xFF, 0xFF, 0xFE, // seq = 0xFFFFFFFE (BE)
        0x00, 0x05, // length = 5 (BE)
        // Payload (5 bytes)
        0x01, 0x02, 0x03, 0x04, 0x05,
    ]);

    var result2 = encode({
        version: 0x02,
        pktType: PKT_SYSTEM_CMD,
        tgtType: TGT_UNICAST,
        roomId: 'roomid',
        userId: 'useridid',
        seq: 0xFFFFFFFE,
        payload: new Uint8Array([0x01, 0x02, 0x03, 0x04, 0x05]),
    });

    if (!arraysEqual(result2, expected2)) {
        throw new Error(
            'Test vector 2 mismatch:\n' +
            '  expected: ' + toHex(expected2) + '\n' +
            '  got:      ' + toHex(result2)
        );
    }

    // Round-trip decode
    var decoded2 = decode(expected2);
    if (decoded2.version !== 2) throw new Error('TV2 version mismatch');
    if (decoded2.pktType !== PKT_SYSTEM_CMD) throw new Error('TV2 pktType mismatch');
    if (decoded2.tgtType !== TGT_UNICAST) throw new Error('TV2 tgtType mismatch');
    if (decoded2.roomId !== 'roomid') throw new Error('TV2 roomId mismatch');
    if (decoded2.userId !== 'useridid') throw new Error('TV2 userId mismatch');
    if (decoded2.seq !== 0xFFFFFFFE) throw new Error('TV2 seq mismatch');
    if (!arraysEqual(decoded2.payload, new Uint8Array([0x01, 0x02, 0x03, 0x04, 0x05]))) throw new Error('TV2 payload mismatch');

    // ── 测试向量 3：对应 Rust 测试 round_trip_basic（仅验证头部） ──
    //   PacketType::RawMotion (0x01), TargetType::Broadcast (0x01)
    //   room_id="ninja1", user_id="player_A", seq=42, payload_len=0
    var expected3 = new Uint8Array([
        0x02, // version
        0x01, // pktType (RawMotion)
        0x01, // tgtType (Broadcast)
        0x00, // reserved
        0x6E, 0x69, 0x6E, 0x6A, 0x61, 0x31, // roomId "ninja1"
        0x70, 0x6C, 0x61, 0x79, 0x65, 0x72, 0x5F, 0x41, // userId "player_A"
        0x00, 0x00, 0x00, 0x2A, // seq = 42 (BE)
        0x00, 0x00, // length = 0 (BE)
    ]);

    var result3 = encode({
        version: 0x02,
        pktType: PKT_RAW_MOTION,
        tgtType: TGT_BROADCAST,
        roomId: 'ninja1',
        userId: 'player_A',
        seq: 42,
        payload: new Uint8Array([]),
    });

    if (!arraysEqual(result3, expected3)) {
        throw new Error(
            'Test vector 3 mismatch:\n' +
            '  expected: ' + toHex(expected3) + '\n' +
            '  got:      ' + toHex(result3)
        );
    }

    // ── 错误路径测试 ──

    // 版本不匹配
    try {
        var badVer = new Uint8Array(24);
        badVer[0] = 0x99;
        badVer[1] = 0x04;
        badVer[2] = 0x01;
        decode(badVer);
        throw new Error('should have thrown for wrong version');
    } catch (e) {
        if (!(e instanceof DecodeError)) throw e;
    }

    // 长度不足
    try {
        decode(new Uint8Array(10));
        throw new Error('should have thrown for too short');
    } catch (e) {
        if (!(e instanceof DecodeError)) throw e;
    }

    // 未知包类型
    try {
        var badPkt = new Uint8Array(24);
        badPkt[0] = 0x02;
        badPkt[1] = 0xFF;
        badPkt[2] = 0x01;
        decode(badPkt);
        throw new Error('should have thrown for unknown pktType');
    } catch (e) {
        if (!(e instanceof DecodeError)) throw e;
    }

    // 未知目标类型
    try {
        var badTgt = new Uint8Array(24);
        badTgt[0] = 0x02;
        badTgt[1] = 0x01;
        badTgt[2] = 0xFF;
        decode(badTgt);
        throw new Error('should have thrown for unknown tgtType');
    } catch (e) {
        if (!(e instanceof DecodeError)) throw e;
    }

    // roomId 过长
    try {
        encode({
            version: 0x02,
            pktType: PKT_HEARTBEAT,
            tgtType: TGT_BROADCAST,
            roomId: 'toolong123',
            userId: 'u1',
            seq: 0,
            payload: new Uint8Array([]),
        });
        throw new Error('should have thrown for too-long roomId');
    } catch (e) {
        if (!(e instanceof EncodeError)) throw e;
    }

    // roomId 含非 ASCII
    try {
        encode({
            version: 0x02,
            pktType: PKT_HEARTBEAT,
            tgtType: TGT_BROADCAST,
            roomId: '\u623F\u95F4', // 房间
            userId: 'u1',
            seq: 0,
            payload: new Uint8Array([]),
        });
        throw new Error('should have thrown for non-ASCII roomId');
    } catch (e) {
        if (!(e instanceof EncodeError)) throw e;
    }

    console.log('gateway_protocol.js: all self-tests PASSED');
    return true;
}

// ── 内部 helper ─────────────────────────────────────────────────────────

function arraysEqual(a, b) {
    if (a.length !== b.length) return false;
    for (var i = 0; i < a.length; i++) {
        if (a[i] !== b[i]) return false;
    }
    return true;
}

function toHex(arr) {
    var hex = '';
    for (var i = 0; i < arr.length; i++) {
        var h = arr[i].toString(16);
        if (h.length === 1) h = '0' + h;
        hex += h;
    }
    return hex;
}

// ── 导出 ────────────────────────────────────────────────────────────────

// 兼容多种模块系统
if (typeof module !== 'undefined' && module.exports) {
    // CommonJS / Node.js
    module.exports = {
        // constants
        PROTOCOL_VERSION: PROTOCOL_VERSION,
        HEADER_LEN: HEADER_LEN,
        ROOM_ID_LEN: ROOM_ID_LEN,
        USER_ID_LEN: USER_ID_LEN,
        PKT_RAW_MOTION: PKT_RAW_MOTION,
        PKT_AI_EVENT: PKT_AI_EVENT,
        PKT_SYSTEM_CMD: PKT_SYSTEM_CMD,
        PKT_HEARTBEAT: PKT_HEARTBEAT,
        TGT_BROADCAST: TGT_BROADCAST,
        TGT_UNICAST: TGT_UNICAST,
        // errors
        ProtocolError: ProtocolError,
        EncodeError: EncodeError,
        DecodeError: DecodeError,
        // functions
        encode: encode,
        decode: decode,
        selfTest: selfTest,
    };
} else if (typeof define === 'function' && define.amd) {
    // AMD
    define(function () {
        return {
            PROTOCOL_VERSION: PROTOCOL_VERSION,
            HEADER_LEN: HEADER_LEN,
            ROOM_ID_LEN: ROOM_ID_LEN,
            USER_ID_LEN: USER_ID_LEN,
            PKT_RAW_MOTION: PKT_RAW_MOTION,
            PKT_AI_EVENT: PKT_AI_EVENT,
            PKT_SYSTEM_CMD: PKT_SYSTEM_CMD,
            PKT_HEARTBEAT: PKT_HEARTBEAT,
            TGT_BROADCAST: TGT_BROADCAST,
            TGT_UNICAST: TGT_UNICAST,
            ProtocolError: ProtocolError,
            EncodeError: EncodeError,
            DecodeError: DecodeError,
            encode: encode,
            decode: decode,
            selfTest: selfTest,
        };
    });
} else {
    // 浏览器全局
    var gatewayProtocol = {
        PROTOCOL_VERSION: PROTOCOL_VERSION,
        HEADER_LEN: HEADER_LEN,
        ROOM_ID_LEN: ROOM_ID_LEN,
        USER_ID_LEN: USER_ID_LEN,
        PKT_RAW_MOTION: PKT_RAW_MOTION,
        PKT_AI_EVENT: PKT_AI_EVENT,
        PKT_SYSTEM_CMD: PKT_SYSTEM_CMD,
        PKT_HEARTBEAT: PKT_HEARTBEAT,
        TGT_BROADCAST: TGT_BROADCAST,
        TGT_UNICAST: TGT_UNICAST,
        ProtocolError: ProtocolError,
        EncodeError: EncodeError,
        DecodeError: DecodeError,
        encode: encode,
        decode: decode,
        selfTest: selfTest,
    };
    if (typeof window !== 'undefined') {
        window.gatewayProtocol = gatewayProtocol;
    }
    if (typeof globalThis !== 'undefined') {
        globalThis.gatewayProtocol = gatewayProtocol;
    }
}

// 直接作为脚本运行时自动执行自测
if (typeof require !== 'undefined' && require.main === module) {
    try {
        selfTest();
        console.log('gateway_protocol.js: all self-tests PASSED');
    } catch (e) {
        console.error('gateway_protocol.js: self-test FAILED —', e.message);
        process.exit(1);
    }
}
