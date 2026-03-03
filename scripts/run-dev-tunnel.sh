#!/usr/bin/env bash
set -Eeuo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT_DIR}"

LOCAL_ENV_FILE="${LOCAL_ENV_FILE:-${ROOT_DIR}/.env.local}"

load_local_env() {
  if [[ -f "${LOCAL_ENV_FILE}" ]]; then
    set -a
    # shellcheck disable=SC1090
    source "${LOCAL_ENV_FILE}"
    set +a
  fi
}

load_local_env

SSH_TARGET="${1:-${SSH_TARGET:-}}"
if [[ -z "${SSH_TARGET}" ]]; then
  cat <<'USAGE'
Usage:
  ./scripts/run-dev-tunnel.sh [ssh_user@vps_host]

Environment variables:
  SSH_TARGET=<ssh_user@vps_host>
  SSH_KEY_PATH=<path_to_private_key>
  PUBLIC_HOST=<public_https_host>
  LOCAL_FRONTEND_PORT=5173
  LOCAL_BACKEND_PORT=3000
  REMOTE_FRONTEND_INTERNAL_PORT=45173
  REMOTE_BACKEND_INTERNAL_PORT=43000
  INSTALL_FRONTEND_DEPS=1

Optional local env file:
  .env.local (or LOCAL_ENV_FILE=<path>)

Requires (one-time):
  ./scripts/install-remote-caddy.sh [ssh_user@vps_host]

After start, open:
  https://<PUBLIC_HOST>
USAGE
  exit 1
fi

SSH_KEY_PATH="${SSH_KEY_PATH:-}"
PUBLIC_HOST="${PUBLIC_HOST:-}"

LOCAL_FRONTEND_PORT="${LOCAL_FRONTEND_PORT:-5173}"
LOCAL_BACKEND_PORT="${LOCAL_BACKEND_PORT:-3000}"

REMOTE_FRONTEND_INTERNAL_PORT="${REMOTE_FRONTEND_INTERNAL_PORT:-45173}"
REMOTE_BACKEND_INTERNAL_PORT="${REMOTE_BACKEND_INTERNAL_PORT:-43000}"

INSTALL_FRONTEND_DEPS="${INSTALL_FRONTEND_DEPS:-1}"

BACKEND_PID=""
FRONTEND_PID=""
TUNNEL_PID=""
SSH_KEY_TEMP=""

log() {
  printf '[tunnel] %s\n' "$*"
}

die() {
  printf '[tunnel] ERROR: %s\n' "$*" >&2
  exit 1
}

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "Missing required command: $1"
}

require_non_empty() {
  local value="$1"
  local name="$2"
  if [[ -z "${value}" ]]; then
    die "${name} is required (set it in env/.env.local or pass SSH target as arg)"
  fi
}

prepare_ssh_key() {
  SSH_KEY_TEMP="$(mktemp)"
  cp "${SSH_KEY_PATH}" "${SSH_KEY_TEMP}"
  chmod 600 "${SSH_KEY_TEMP}"
}

validate_port() {
  local value="$1"
  local name="$2"
  if [[ ! "${value}" =~ ^[0-9]+$ ]]; then
    die "${name} must be an integer port number, got: ${value}"
  fi
  if ((value < 1 || value > 65535)); then
    die "${name} must be between 1 and 65535, got: ${value}"
  fi
}

pids_listening_on_port() {
  local port="$1"

  if command -v lsof >/dev/null 2>&1; then
    lsof -tiTCP:"${port}" -sTCP:LISTEN 2>/dev/null | sort -u
    return 0
  fi

  if command -v ss >/dev/null 2>&1; then
    ss -ltnp "sport = :${port}" 2>/dev/null \
      | sed -n 's/.*pid=\([0-9]\+\).*/\1/p' \
      | sort -u
    return 0
  fi

  if command -v fuser >/dev/null 2>&1; then
    fuser -n tcp "${port}" 2>/dev/null \
      | tr ' ' '\n' \
      | grep -E '^[0-9]+$' \
      | sort -u
    return 0
  fi

  die "Cannot detect port users: install one of lsof, ss, or fuser"
}

kill_processes_on_port() {
  local port="$1"
  local pids
  pids="$(pids_listening_on_port "${port}" | tr '\n' ' ' | xargs 2>/dev/null || true)"
  if [[ -z "${pids}" ]]; then
    return 0
  fi

  log "Killing processes using port ${port}: ${pids}"
  kill ${pids} 2>/dev/null || true
  sleep 1

  local alive=""
  for pid in ${pids}; do
    if kill -0 "${pid}" 2>/dev/null; then
      alive="${alive} ${pid}"
    fi
  done

  if [[ -n "${alive// }" ]]; then
    log "Force killing remaining processes on port ${port}:${alive}"
    kill -9 ${alive} 2>/dev/null || true
    sleep 0.2
  fi

  if [[ -n "$(pids_listening_on_port "${port}" | head -n 1)" ]]; then
    die "Port ${port} is still in use after cleanup"
  fi
}

cleanup() {
  local pids=()
  [[ -n "${TUNNEL_PID}" ]] && pids+=("${TUNNEL_PID}")
  [[ -n "${FRONTEND_PID}" ]] && pids+=("${FRONTEND_PID}")
  [[ -n "${BACKEND_PID}" ]] && pids+=("${BACKEND_PID}")

  if [[ ${#pids[@]} -gt 0 ]]; then
    log "Stopping local services and tunnel..."
    kill "${pids[@]}" 2>/dev/null || true
    wait "${pids[@]}" 2>/dev/null || true
  fi

  if [[ -n "${SSH_KEY_TEMP}" && -f "${SSH_KEY_TEMP}" ]]; then
    rm -f "${SSH_KEY_TEMP}"
  fi
}
trap cleanup EXIT INT TERM

wait_for_local_port() {
  local port="$1"
  local name="$2"
  local pid="$3"
  local max_checks=120

  for ((i=0; i<max_checks; i++)); do
    if (echo >"/dev/tcp/127.0.0.1/${port}") >/dev/null 2>&1; then
      return 0
    fi
    if ! kill -0 "${pid}" 2>/dev/null; then
      die "${name} exited before listening on 127.0.0.1:${port}"
    fi
    sleep 0.5
  done

  die "Timed out waiting for ${name} on 127.0.0.1:${port}"
}

require_cmd ssh
require_cmd cargo
require_cmd npm

require_non_empty "${SSH_TARGET}" "SSH_TARGET"
require_non_empty "${SSH_KEY_PATH}" "SSH_KEY_PATH"
require_non_empty "${PUBLIC_HOST}" "PUBLIC_HOST"

[[ -f "${SSH_KEY_PATH}" ]] || die "SSH key not found: ${SSH_KEY_PATH}"
prepare_ssh_key

validate_port "${LOCAL_FRONTEND_PORT}" "LOCAL_FRONTEND_PORT"
validate_port "${LOCAL_BACKEND_PORT}" "LOCAL_BACKEND_PORT"
validate_port "${REMOTE_FRONTEND_INTERNAL_PORT}" "REMOTE_FRONTEND_INTERNAL_PORT"
validate_port "${REMOTE_BACKEND_INTERNAL_PORT}" "REMOTE_BACKEND_INTERNAL_PORT"

log "Ensuring local ports are free..."
kill_processes_on_port "${LOCAL_BACKEND_PORT}"
kill_processes_on_port "${LOCAL_FRONTEND_PORT}"

if [[ "${INSTALL_FRONTEND_DEPS}" == "1" && ! -d frontend/node_modules ]]; then
  log "Installing frontend dependencies (npm ci)..."
  (cd frontend && npm ci)
fi

log "Starting backend on 127.0.0.1:${LOCAL_BACKEND_PORT}..."
(
  cd backend
  if [[ -f .env ]]; then
    set -a
    # shellcheck disable=SC1091
    source .env
    set +a
  fi

  APP_ADDR="127.0.0.1:${LOCAL_BACKEND_PORT}" \
  FRONTEND_ORIGIN="https://${PUBLIC_HOST}" \
  PUBLIC_BASE_URL="https://${PUBLIC_HOST}" \
  cargo run
) &
BACKEND_PID="$!"

wait_for_local_port "${LOCAL_BACKEND_PORT}" "backend" "${BACKEND_PID}"

log "Starting frontend on 127.0.0.1:${LOCAL_FRONTEND_PORT}..."
(
  cd frontend
  EXTRA_ALLOWED_HOSTS="${VITE_ALLOWED_HOSTS:-}"
  if [[ -n "${EXTRA_ALLOWED_HOSTS}" ]]; then
    EXTRA_ALLOWED_HOSTS="${PUBLIC_HOST},${EXTRA_ALLOWED_HOSTS}"
  else
    EXTRA_ALLOWED_HOSTS="${PUBLIC_HOST}"
  fi
  VITE_API_BASE_URL="https://${PUBLIC_HOST}" \
  VITE_ALLOWED_HOSTS="${EXTRA_ALLOWED_HOSTS}" \
  npm run dev -- --host 127.0.0.1 --port "${LOCAL_FRONTEND_PORT}" --strictPort
) &
FRONTEND_PID="$!"

wait_for_local_port "${LOCAL_FRONTEND_PORT}" "frontend" "${FRONTEND_PID}"

log "Opening SSH reverse tunnel via ${SSH_TARGET}..."
log "Public app URL: https://${PUBLIC_HOST}"

(
  ssh \
    -i "${SSH_KEY_TEMP}" \
    -o ConnectTimeout=15 \
    -o ExitOnForwardFailure=yes \
    -o ServerAliveInterval=30 \
    -o ServerAliveCountMax=3 \
    -o StrictHostKeyChecking=accept-new \
    -N \
    -R "127.0.0.1:${REMOTE_FRONTEND_INTERNAL_PORT}:127.0.0.1:${LOCAL_FRONTEND_PORT}" \
    -R "127.0.0.1:${REMOTE_BACKEND_INTERNAL_PORT}:127.0.0.1:${LOCAL_BACKEND_PORT}" \
    "${SSH_TARGET}"
) &
TUNNEL_PID="$!"

wait -n "${BACKEND_PID}" "${FRONTEND_PID}" "${TUNNEL_PID}" || true
die "A process exited unexpectedly. Check logs above."
