#!/usr/bin/env bash
set -Eeuo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT_DIR}"

DEFAULT_TUNNEL_ENV_FILE="${ROOT_DIR}/.env.tunnel"
LEGACY_TUNNEL_ENV_FILE="${ROOT_DIR}/.env.tunnel.local"
FALLBACK_ENV_FILE="${ROOT_DIR}/.env.local"
if [[ -n "${LOCAL_ENV_FILE:-}" ]]; then
  LOCAL_ENV_FILE="${LOCAL_ENV_FILE}"
elif [[ -f "${DEFAULT_TUNNEL_ENV_FILE}" ]]; then
  LOCAL_ENV_FILE="${DEFAULT_TUNNEL_ENV_FILE}"
elif [[ -f "${LEGACY_TUNNEL_ENV_FILE}" ]]; then
  LOCAL_ENV_FILE="${LEGACY_TUNNEL_ENV_FILE}"
else
  LOCAL_ENV_FILE="${FALLBACK_ENV_FILE}"
fi
TEMPLATE_DIR="${ROOT_DIR}/deploy/templates"
TUNNEL_CADDY_TEMPLATE="${TEMPLATE_DIR}/caddy.tunnel.Caddyfile"

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
  ./scripts/install-remote-caddy.sh [ssh_user@vps_host]

Environment variables:
  SSH_TARGET=<ssh_user@vps_host>
  SSH_KEY_PATH=<path_to_private_key>
  PUBLIC_HOST=<public_https_host>
  TUNNEL_UPSTREAM_HOST=127.0.0.1
  REMOTE_FRONTEND_INTERNAL_PORT=45173
  REMOTE_BACKEND_INTERNAL_PORT=43000

Optional local env file:
  .env.tunnel (fallback: .env.tunnel.local, .env.local, or LOCAL_ENV_FILE=<path>)
USAGE
  exit 1
fi

SSH_KEY_PATH="${SSH_KEY_PATH:-}"
PUBLIC_HOST="${PUBLIC_HOST:-}"
TUNNEL_UPSTREAM_HOST="${TUNNEL_UPSTREAM_HOST:-127.0.0.1}"
REMOTE_FRONTEND_INTERNAL_PORT="${REMOTE_FRONTEND_INTERNAL_PORT:-45173}"
REMOTE_BACKEND_INTERNAL_PORT="${REMOTE_BACKEND_INTERNAL_PORT:-43000}"
SSH_KEY_TEMP=""
CADDYFILE_TEMP=""

require_non_empty() {
  local value="$1"
  local name="$2"
  if [[ -z "${value}" ]]; then
    echo "[remote-setup] ERROR: ${name} is required (set it in local env file or pass SSH target as arg)" >&2
    exit 1
  fi
}

require_file() {
  local path="$1"
  local name="$2"
  if [[ ! -f "${path}" ]]; then
    echo "[remote-setup] ERROR: ${name} not found: ${path}" >&2
    exit 1
  fi
}

escape_sed_replacement() {
  printf '%s' "$1" | sed -e 's/[\\/&]/\\&/g'
}

render_template() {
  local template_path="$1"
  local output_path="$2"
  shift 2

  cp "${template_path}" "${output_path}"
  while (($#)); do
    local key="$1"
    local value="$2"
    local escaped_value
    shift 2
    escaped_value="$(escape_sed_replacement "${value}")"
    sed -i "s|__${key}__|${escaped_value}|g" "${output_path}"
  done
}

require_non_empty "${SSH_TARGET}" "SSH_TARGET"
require_non_empty "${SSH_KEY_PATH}" "SSH_KEY_PATH"
require_non_empty "${PUBLIC_HOST}" "PUBLIC_HOST"
require_non_empty "${TUNNEL_UPSTREAM_HOST}" "TUNNEL_UPSTREAM_HOST"
require_file "${TUNNEL_CADDY_TEMPLATE}" "Tunnel Caddy template"

[[ -f "${SSH_KEY_PATH}" ]] || {
  echo "[remote-setup] ERROR: SSH key not found at ${SSH_KEY_PATH}" >&2
  exit 1
}

cleanup() {
  if [[ -n "${SSH_KEY_TEMP}" && -f "${SSH_KEY_TEMP}" ]]; then
    rm -f "${SSH_KEY_TEMP}"
  fi
  if [[ -n "${CADDYFILE_TEMP}" && -f "${CADDYFILE_TEMP}" ]]; then
    rm -f "${CADDYFILE_TEMP}"
  fi
}
trap cleanup EXIT INT TERM

SSH_KEY_TEMP="$(mktemp)"
cp "${SSH_KEY_PATH}" "${SSH_KEY_TEMP}"
chmod 600 "${SSH_KEY_TEMP}"

CADDYFILE_TEMP="$(mktemp)"
render_template \
  "${TUNNEL_CADDY_TEMPLATE}" \
  "${CADDYFILE_TEMP}" \
  "PUBLIC_HOST" "${PUBLIC_HOST}" \
  "TUNNEL_UPSTREAM_HOST" "${TUNNEL_UPSTREAM_HOST}" \
  "REMOTE_BACKEND_INTERNAL_PORT" "${REMOTE_BACKEND_INTERNAL_PORT}" \
  "REMOTE_FRONTEND_INTERNAL_PORT" "${REMOTE_FRONTEND_INTERNAL_PORT}"

REMOTE_CADDYFILE="/tmp/qstream-install-caddy-$(date -u +%Y%m%d%H%M%S).Caddyfile"
scp \
  -i "${SSH_KEY_TEMP}" \
  -o ConnectTimeout=15 \
  -o StrictHostKeyChecking=accept-new \
  "${CADDYFILE_TEMP}" "${SSH_TARGET}:${REMOTE_CADDYFILE}"

ssh \
  -i "${SSH_KEY_TEMP}" \
  -o ConnectTimeout=15 \
  -o StrictHostKeyChecking=accept-new \
  "${SSH_TARGET}" \
  "PUBLIC_HOST='${PUBLIC_HOST}' TUNNEL_UPSTREAM_HOST='${TUNNEL_UPSTREAM_HOST}' REMOTE_CADDYFILE='${REMOTE_CADDYFILE}' bash -s" <<'REMOTE_SCRIPT'
set -Eeuo pipefail

if ! command -v sudo >/dev/null 2>&1; then
  echo "[remote-setup] sudo is required on remote host" >&2
  exit 1
fi

if ! command -v caddy >/dev/null 2>&1; then
  echo "[remote-setup] Caddy is not installed. Installing..."
  sudo apt-get update
  sudo apt-get install -y debian-keyring debian-archive-keyring apt-transport-https curl gnupg
  curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' | sudo gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
  curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' | sudo tee /etc/apt/sources.list.d/caddy-stable.list >/dev/null
  sudo apt-get update
  sudo apt-get install -y caddy
else
  echo "[remote-setup] Caddy already installed. Skipping installation."
fi

# Caddy must own :80/:443 for automatic HTTPS.
if command -v nginx >/dev/null 2>&1; then
  if systemctl is-enabled --quiet nginx 2>/dev/null || systemctl is-active --quiet nginx 2>/dev/null; then
    echo "[remote-setup] Disabling nginx so Caddy can bind ports 80/443..."
    sudo systemctl stop nginx || true
    sudo systemctl disable nginx || true
  fi
fi

sudo mv "${REMOTE_CADDYFILE}" /etc/caddy/Caddyfile
sudo chmod 0644 /etc/caddy/Caddyfile

sudo caddy fmt --overwrite /etc/caddy/Caddyfile
sudo caddy validate --config /etc/caddy/Caddyfile
sudo systemctl enable caddy
sudo systemctl restart caddy
sudo systemctl --no-pager --full status caddy | sed -n '1,40p'
sudo ss -ltnp | sed -n '1,30p'

echo "[remote-setup] Caddy configured for https://${PUBLIC_HOST} (tunnel upstream: ${TUNNEL_UPSTREAM_HOST})"
REMOTE_SCRIPT
