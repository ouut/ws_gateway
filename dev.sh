#!/bin/bash
set -euo pipefail

IMAGE="codercom/code-server:latest"

# ── Defaults (used by start/restart) ──────────────────────────
PORT=8080
PASSWORD="cc"
TARGET_DIR="$(pwd)"

# ── Container naming ──────────────────────────────────────────
# Derive a unique container name from the project directory path.
# e.g. /home/user/my-project  →  webit-my-project
#      /                      →  webit-root
#      /home/user/my project  →  webit-my-project
sanitize_name() {
    local raw="${1##*/}"                     # basename
    raw="${raw:-root}"                       # "/" → "root"
    echo "webit-${raw}" \
        | sed 's/[^a-zA-Z0-9_.-]/-/g' \
        | sed 's/--*/-/g' \
        | sed 's/^-//;s/-$//'
}

CONTAINER_NAME="$(sanitize_name "${TARGET_DIR}")"

# ── Helpers ───────────────────────────────────────────────────

usage() {
    cat <<'HELP'
Webit — One-command browser VS Code (code-server)
==================================================

🚀 Command reference
──────────────────────────────────────────────────
  ./run.sh start    Start container (default: port 8080, password cc, pwd)
  ./run.sh stop     Stop container (preserves all data)
  ./run.sh restart  Stop then recreate (accepts new options)
  ./run.sh remove   Destroy container permanently
  ./run.sh status   Show container status
  ./run.sh logs     Tail container logs in real time
  ./run.sh help     Show this help

📋 Start / restart options
──────────────────────────────────────────────────
  --port PORT           Listen port (default: 8080)
  --password PASSWORD   Login password (default: cc)
  --dir DIR_PATH        Project directory to share (absolute path, default: pwd)

💡 Examples
──────────────────────────────────────────────────
  # Quick start (defaults)
  ./run.sh start

  # Custom port, password & directory
  ./run.sh start --port 9090 --password 123456 --dir /home/user/my-project

  # Stop & resume
  ./run.sh stop
  ./run.sh start          # extensions, settings, sessions all preserved

  # Change port on the fly
  ./run.sh restart --port 9000

  # Check status / logs
  ./run.sh status
  ./run.sh logs

🌐 Remote one-liner (curl pipe)
──────────────────────────────────────────────────
  curl -sSL https://raw.githubusercontent.com/ouut/webit/main/run.sh | bash

  # With options
  curl -sSL ... | bash -s -- --port 9090 --password 123456 --dir /home/user/project

🔧 Container naming
──────────────────────────────────────────────────
  Name = webit-<dirname>
  e.g. /home/user/my-app  →  container name webit-my-app
  Run multiple instances side-by-side in different directories.

💾 Data persistence
──────────────────────────────────────────────────
  All code-server user data   (extensions, config, cache, sessions, /root home)
  lives in .code-server-home/ inside your project directory.
  Container removal does not delete it; rebuild restores everything.

  ⚠️  Add .code-server-home to your .gitignore

🛡️ Permission safety
──────────────────────────────────────────────────
  Automatically detects root vs. rootless Docker and maps users so
  files created inside the container belong to you — never to root.

🔄 Anti-exit (linger + restart)
──────────────────────────────────────────────────
  Rootless Docker runs inside your systemd user session.  Closing the
  terminal would normally SIGTERM the container → Exit 143.

  This script fixes it two ways:
  1. Automatically runs `loginctl enable-linger` so your session
     persists after the terminal closes.
  2. Adds `--restart unless-stopped` so the container auto-recovers
     even if SIGTERM slips through.
HELP
    exit 0
}

detect_docker_user() {
    if docker info 2>/dev/null | grep -q 'rootless'; then
        DOCKER_USER="0:0"
        echo "🐳 Docker mode: rootless (auto-detected)"
    else
        DOCKER_USER="$(id -u):$(id -g)"
        echo "🐳 Docker mode: root (auto-detected)"
    fi
}

# Enable lingering so systemd won't kill the Docker daemon (and thus
# containers) when the user's terminal / SSH session ends.  Without
# this, closing the terminal sends SIGTERM → container exits with 143.
# Rootless Docker is especially vulnerable because the daemon runs
# inside the user's systemd session slice.
ensure_linger() {
    if command -v loginctl >/dev/null 2>&1; then
        local LINGER
        LINGER=$(loginctl show-user "$(whoami)" --property=Linger 2>/dev/null | cut -d= -f2)
        if [[ "${LINGER}" != "yes" ]]; then
            loginctl enable-linger 2>/dev/null || true
            if loginctl show-user "$(whoami)" --property=Linger 2>/dev/null | grep -q '=yes'; then
                echo "🔒 Enabled linger for user $(whoami) — containers survive terminal close"
            else
                echo "⚠️  Unable to enable linger. Close terminal → container may exit (143)."
                echo "   Run manually:  loginctl enable-linger"
            fi
        fi
    fi
}

container_exists() {
    docker ps -a --format '{{.Names}}' | grep -q "^${CONTAINER_NAME}$"
}

container_running() {
    docker ps --format '{{.Names}}' | grep -q "^${CONTAINER_NAME}$"
}

# ── Commands ──────────────────────────────────────────────────

cmd_start() {
    # Parse options
    while [[ $# -gt 0 ]]; do
        case $1 in
            --port)     PORT="$2"; shift 2 ;;
            --password) PASSWORD="$2"; shift 2 ;;
            --dir)      TARGET_DIR="$2"; shift 2 ;;
            *) echo "Unknown option: $1"; usage ;;
        esac
    done

    # Recompute container name in case --dir changed the target
    CONTAINER_NAME="$(sanitize_name "${TARGET_DIR}")"

    # If already running, just report
    if container_running; then
        local HOST_PORT
        HOST_PORT=$(docker port "${CONTAINER_NAME}" 8080 2>/dev/null | head -1 | sed 's/.*://')
        echo "⚠️  Container '${CONTAINER_NAME}' is already running."
        echo "   Access via: http://localhost:${HOST_PORT:-?}"
        echo "   Use './run.sh restart' to apply new config, or './run.sh stop' first."
        exit 0
    fi

    detect_docker_user
    ensure_linger

    # Create persistent data directories on the host.
    # /home/coder and /root both survive container removal, so tools
    # that write to /root (e.g. Codewhale) don't lose state.
    local DATA_DIR="${TARGET_DIR}/.code-server-home"
    mkdir -p "${DATA_DIR}" "${DATA_DIR}/root-home"

    # Clean up any stopped container so we can recreate with fresh config
    docker rm -f "${CONTAINER_NAME}" 2>/dev/null || true

    echo "🚀 Starting lightweight Cloud IDE (code-server)..."
    echo "📂 Workspace Root:    ${TARGET_DIR}"
    echo "💾 Full Home Data:    ${DATA_DIR} -> /home/coder"
    echo "👤 Root Home Data:    ${DATA_DIR}/root-home -> /root"
    echo "🌐 Access Port:       ${PORT}"
    echo "🔑 Access Password:   ${PASSWORD}"

    # Nested mount: entire /home/coder is persisted, project is a sub-mount.
    # CODE_SERVER_RECONNECTION_GRACE_TIME=2592000 keeps sessions alive for 30 days.
    docker run -d \
        --name "${CONTAINER_NAME}" \
        --user "${DOCKER_USER}" \
        --restart unless-stopped \
        -p "${PORT}:8080" \
        -v "${DATA_DIR}":/home/coder \
        -v "${DATA_DIR}/root-home":/root \
        -v "${TARGET_DIR}":/home/coder/project \
        -e "PASSWORD=${PASSWORD}" \
        -e "CODE_SERVER_RECONNECTION_GRACE_TIME=2592000" \
        "${IMAGE}" \
        --bind-addr 0.0.0.0:8080 /home/coder/project

    echo "------------------------------------------------"
    echo "✅ Started successfully! Access via: http://localhost:${PORT}"
    echo "🛑 To stop:  ./run.sh stop   |   ▶️  To resume: ./run.sh start"
    echo "------------------------------------------------"
}

cmd_stop() {
    if container_running; then
        echo "🛑 Stopping container '${CONTAINER_NAME}'..."
        docker stop "${CONTAINER_NAME}"
        echo "✅ Container stopped. Use './run.sh start' to resume."
    else
        echo "⚠️  Container '${CONTAINER_NAME}' is not running."
    fi
}

cmd_restart() {
    # Stop if running, then start with any new options
    if container_running; then
        echo "🛑 Stopping container..."
        docker stop "${CONTAINER_NAME}"
    fi
    cmd_start "$@"
}

cmd_remove() {
    if container_exists; then
        echo "🗑️  Removing container '${CONTAINER_NAME}'..."
        docker rm -f "${CONTAINER_NAME}" 2>/dev/null || true
        echo "✅ Container removed."
    else
        echo "⚠️  Container '${CONTAINER_NAME}' does not exist."
    fi
}

cmd_status() {
    if container_running; then
        echo "📊 Container '${CONTAINER_NAME}' status:"
        docker ps --format 'table {{.Names}}\t{{.Status}}\t{{.Ports}}' | head -1
        docker ps --format 'table {{.Names}}\t{{.Status}}\t{{.Ports}}' | grep "^${CONTAINER_NAME}"
    elif container_exists; then
        echo "⏸️  Container '${CONTAINER_NAME}' exists but is stopped."
        echo "   Use './run.sh start' to resume, or './run.sh remove' to delete."
    else
        echo "❌ Container '${CONTAINER_NAME}' does not exist."
        echo "   Use './run.sh start' to create and launch it."
    fi
}

cmd_logs() {
    if container_exists; then
        docker logs --tail 50 -f "${CONTAINER_NAME}"
    else
        echo "❌ Container '${CONTAINER_NAME}' does not exist."
    fi
}

# ═══════════════════════════════════════════════════════════════
# Main entry point — dispatch on first argument
# ═══════════════════════════════════════════════════════════════
COMMAND="${1:-}"

case "${COMMAND}" in
    start)   shift; cmd_start "$@" ;;
    stop)    cmd_stop ;;
    restart) shift; cmd_restart "$@" ;;
    remove)  cmd_remove ;;
    status)  cmd_status ;;
    logs)    cmd_logs ;;
    -h|--help|help) usage ;;
    "")
        cmd_start
        ;;
    --*)
        cmd_start "$@"
        ;;
    *)
        echo "Unknown command: ${COMMAND}"
        usage
        ;;
esac
