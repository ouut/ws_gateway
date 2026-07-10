#!/usr/bin/env bash
#
# build.sh — 交叉编译并打包 gateway 全平台产物
#
# 用法:
#   ./scripts/build.sh              # 编译所有平台并打包
#   ./scripts/build.sh --no-pack    # 只编译，不打包
#   ./scripts/build.sh --platforms linux-x86_64,windows  # 只编译指定平台
#
# 前置条件:
#   - Rust 工具链 (rustup + cargo)
#   - 交叉编译 target: rustup target add x86_64-pc-windows-gnu aarch64-unknown-linux-gnu
#   - Linux 交叉编译工具链: gcc-aarch64-linux-gnu
#   - Windows 交叉编译工具链: gcc-mingw-w64-x86-64

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
DIST_DIR="$PROJECT_DIR/dist"
PROTOCOL_DIR="$PROJECT_DIR/doc/gateway-protocol"
VERSION="${VERSION:-$(grep '^version' "$PROJECT_DIR/Cargo.toml" | head -1 | sed 's/.*"\(.*\)"/\1/')}"

# ── 参数解析 ──────────────────────────────────────────────────────────────────

NO_PACK=false
SELECTED_PLATFORMS=""

for arg in "$@"; do
    case "$arg" in
        --no-pack) NO_PACK=true ;;
        --platforms=*) SELECTED_PLATFORMS="${arg#*=}" ;;
        *) echo "未知参数: $arg" >&2; exit 1 ;;
    esac
done

# ── 平台定义 ──────────────────────────────────────────────────────────────────

# 格式: "target|binary_name|display_name|ext"
ALL_PLATFORMS=(
    "x86_64-unknown-linux-gnu|gateway|linux-x86_64|"
    "x86_64-pc-windows-gnu|gateway.exe|windows-x86_64|.exe"
    "aarch64-unknown-linux-gnu|gateway|linux-aarch64|"
)

# ── 颜色 ──────────────────────────────────────────────────────────────────────

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m'

log()  { echo -e "${CYAN}[build]${NC} $*"; }
ok()   { echo -e "${GREEN}[  ok]${NC} $*"; }
warn() { echo -e "${YELLOW}[ warn]${NC} $*"; }
err()  { echo -e "${RED}[ FAIL]${NC} $*"; }

# ── 工具检查 ──────────────────────────────────────────────────────────────────

check_tools() {
    log "检查编译工具链..."

    # 加载 Rust 环境（必须先于 cargo 检测）
    if [ -f "$HOME/.cargo/env" ]; then
        source "$HOME/.cargo/env"
    fi

    if ! command -v cargo &>/dev/null; then
        err "cargo 未找到，请先安装 Rust: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
        exit 1
    fi

    local missing_targets=()
    for entry in "${ALL_PLATFORMS[@]}"; do
        IFS='|' read -r target _ _ _ <<< "$entry"
        if ! rustup target list --installed 2>/dev/null | grep -q "^$target$"; then
            missing_targets+=("$target")
        fi
    done

    if [ ${#missing_targets[@]} -gt 0 ]; then
        log "安装缺失的 target: ${missing_targets[*]}"
        rustup target add "${missing_targets[@]}"
    fi

    ok "工具链就绪"
}

# ── 编译 gateway 二进制 ───────────────────────────────────────────────────────

build_gateway() {
    local target="$1" binary="$2" display="$3"
    local src="$PROJECT_DIR/target/$target/release/$binary"

    log "编译 gateway → $display ($target)"

    (cd "$PROJECT_DIR" && cargo build --release --target "$target" 2>&1) | \
        grep -E "Compiling gateway|error|warning: build failed" || true

    if [ -f "$src" ]; then
        local size
        size=$(ls -lh "$src" | awk '{print $5}')
        ok "$display 编译完成 ($size)"
        echo "$src"
    else
        err "$display 编译失败"
        return 1
    fi
}

# ── 打包 ──────────────────────────────────────────────────────────────────────

package() {
    log "打包 → $DIST_DIR"

    mkdir -p "$DIST_DIR"

    # 创建版本目录
    local release_dir="$DIST_DIR/gateway-$VERSION"
    rm -rf "$release_dir"
    mkdir -p "$release_dir"

    for entry in "${ALL_PLATFORMS[@]}"; do
        IFS='|' read -r target binary display ext <<< "$entry"

        # 跳过未选中的平台
        if [ -n "$SELECTED_PLATFORMS" ]; then
            if ! echo "$SELECTED_PLATFORMS" | grep -q "$display"; then
                continue
            fi
        fi

        local src="$PROJECT_DIR/target/$target/release/$binary"
        local dest="$release_dir/gateway-$display$ext"

        if [ -f "$src" ]; then
            cp "$src" "$dest"
            chmod +x "$dest"
            ok "复制 $display"
        else
            warn "$display 未编译，跳过（先不带 --no-pack 运行一次）"
        fi
    done

    # 复制协议文档和 SDK
    cp "$PROTOCOL_DIR/protocol.md" "$release_dir/"
    cp -r "$PROJECT_DIR/client_sdk" "$release_dir/"

    # 写入版本信息
    cat > "$release_dir/VERSION.txt" <<EOF
gateway $VERSION
build date: $(date -u +"%Y-%m-%dT%H:%M:%SZ")
rustc: $(rustc --version)
targets:$(for e in "${ALL_PLATFORMS[@]}"; do IFS='|' read -r t _ d _ <<< "$e"; printf " %s" "$d"; done)
EOF

    # 打包
    local archive="gateway-${VERSION}-$(date +%Y%m%d).tar.gz"
    (cd "$DIST_DIR" && tar czf "$archive" "gateway-$VERSION")
    local arc_size
    arc_size=$(ls -lh "$DIST_DIR/$archive" | awk '{print $5}')

    ok "打包完成: $DIST_DIR/$archive ($arc_size)"

    # 列出内容
    echo ""
    log "包内容:"
    tar tzf "$DIST_DIR/$archive" | head -20
    if [ "$(tar tzf "$DIST_DIR/$archive" | wc -l)" -gt 20 ]; then
        echo "  ... ($(tar tzf "$DIST_DIR/$archive" | wc -l) 个文件)"
    fi
}

# ── 主流程 ────────────────────────────────────────────────────────────────────

main() {
    echo ""
    echo "═══════════════════════════════════════════════════════════════"
    echo "  gateway v$VERSION — 交叉编译"
    echo "═══════════════════════════════════════════════════════════════"
    echo ""

    check_tools

    # 编译
    local failed=0
    for entry in "${ALL_PLATFORMS[@]}"; do
        IFS='|' read -r target binary display _ <<< "$entry"

        # 跳过未选中的平台
        if [ -n "$SELECTED_PLATFORMS" ]; then
            if ! echo "$SELECTED_PLATFORMS" | grep -q "$display"; then
                log "跳过 $display (--platforms 未包含)"
                continue
            fi
        fi

        if ! build_gateway "$target" "$binary" "$display"; then
            failed=$((failed + 1))
        fi
    done

    if [ "$NO_PACK" = true ]; then
        log "跳过打包 (--no-pack)"
    elif [ $failed -eq 0 ]; then
        package
    else
        warn "$failed 个平台编译失败，跳过打包"
        exit 1
    fi

    echo ""
    ok "全部完成"
}

main "$@"
