#!/usr/bin/env bash
set -Eeuo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT_DIR}"

LOCAL_ENV_FILE="${LOCAL_ENV_FILE:-${ROOT_DIR}/.env.local}"
TEMPLATE_DIR="${ROOT_DIR}/deploy/templates"
PRODUCTION_CADDY_TEMPLATE="${TEMPLATE_DIR}/caddy.production.Caddyfile"
BACKEND_SERVICE_TEMPLATE="${TEMPLATE_DIR}/qstream-backend.service"
JOURNALD_TEMPLATE="${TEMPLATE_DIR}/qstream-journald.conf"

load_local_env() {
  if [[ -f "${LOCAL_ENV_FILE}" ]]; then
    set -a
    # shellcheck disable=SC1090
    source "${LOCAL_ENV_FILE}"
    set +a
  fi
}

load_local_env

DEPLOY_SSH_TARGET="${1:-${DEPLOY_SSH_TARGET:-${SSH_TARGET:-}}}"
DEPLOY_PUBLIC_HOST="${DEPLOY_PUBLIC_HOST:-${PUBLIC_HOST:-}}"
DEPLOY_SSH_KEY_PATH="${DEPLOY_SSH_KEY_PATH:-${SSH_KEY_PATH:-}}"

DEPLOY_REMOTE_DIR="${DEPLOY_REMOTE_DIR:-/opt/qstream}"
DEPLOY_SYSTEM_USER="${DEPLOY_SYSTEM_USER:-qstream}"
DEPLOY_BACKEND_PORT="${DEPLOY_BACKEND_PORT:-3000}"

DEPLOY_FRONTEND_ORIGIN="${DEPLOY_FRONTEND_ORIGIN:-https://${DEPLOY_PUBLIC_HOST}}"
DEPLOY_PUBLIC_BASE_URL="${DEPLOY_PUBLIC_BASE_URL:-https://${DEPLOY_PUBLIC_HOST}}"
DEPLOY_DATABASE_URL="${DEPLOY_DATABASE_URL:-sqlite:///var/lib/qstream/qstream.db?mode=rwc}"

DEPLOY_GOOGLE_CLIENT_ID="${DEPLOY_GOOGLE_CLIENT_ID:-${GOOGLE_CLIENT_ID:-}}"
DEPLOY_GOOGLE_CLIENT_SECRET="${DEPLOY_GOOGLE_CLIENT_SECRET:-${GOOGLE_CLIENT_SECRET:-}}"
DEPLOY_GOOGLE_REDIRECT_URI="${DEPLOY_GOOGLE_REDIRECT_URI:-${GOOGLE_REDIRECT_URI:-https://${DEPLOY_PUBLIC_HOST}/api/google_oauth2}}"

INSTALL_FRONTEND_DEPS="${INSTALL_FRONTEND_DEPS:-1}"
SKIP_BUILD="${SKIP_BUILD:-0}"
DRY_RUN="${DRY_RUN:-0}"
DEPLOY_RUST_TARGET="${DEPLOY_RUST_TARGET:-x86_64-unknown-linux-musl}"

SSH_KEY_TEMP=""
BUNDLE_DIR=""
BUNDLE_FILE=""

log() {
  printf '[deploy] %s\n' "$*"
}

die() {
  printf '[deploy] ERROR: %s\n' "$*" >&2
  exit 1
}

cleanup() {
  if [[ -n "${SSH_KEY_TEMP}" && -f "${SSH_KEY_TEMP}" ]]; then
    rm -f "${SSH_KEY_TEMP}"
  fi
  if [[ -n "${BUNDLE_FILE}" && -f "${BUNDLE_FILE}" ]]; then
    rm -f "${BUNDLE_FILE}"
  fi
  if [[ -n "${BUNDLE_DIR}" && -d "${BUNDLE_DIR}" ]]; then
    rm -rf "${BUNDLE_DIR}"
  fi
}
trap cleanup EXIT INT TERM

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "Missing required command: $1"
}

require_non_empty() {
  local value="$1"
  local name="$2"
  if [[ -z "${value}" ]]; then
    die "${name} is required (set env/.env.local or pass ssh target argument)"
  fi
}

require_file() {
  local path="$1"
  local name="$2"
  if [[ ! -f "${path}" ]]; then
    die "${name} not found: ${path}"
  fi
}

assert_single_line() {
  local value="$1"
  local name="$2"
  if [[ "${value}" == *$'\n'* ]]; then
    die "${name} must be single-line value"
  fi
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

target_arch_from_rust_target() {
  local target="$1"
  case "${target}" in
    x86_64-*) printf 'x86_64\n' ;;
    aarch64-*) printf 'aarch64\n' ;;
    armv7-*) printf 'armv7l\n' ;;
    i686-*) printf 'i686\n' ;;
    *) return 1 ;;
  esac
}

prepare_ssh_key() {
  SSH_KEY_TEMP="$(mktemp)"
  cp "${DEPLOY_SSH_KEY_PATH}" "${SSH_KEY_TEMP}"
  chmod 600 "${SSH_KEY_TEMP}"
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

write_backend_env_file() {
  local file="$1"
  : > "${file}"
  printf 'APP_ADDR=127.0.0.1:%s\n' "${DEPLOY_BACKEND_PORT}" >> "${file}"
  printf 'FRONTEND_ORIGIN=%s\n' "${DEPLOY_FRONTEND_ORIGIN}" >> "${file}"
  printf 'PUBLIC_BASE_URL=%s\n' "${DEPLOY_PUBLIC_BASE_URL}" >> "${file}"
  printf 'DATABASE_URL=%s\n' "${DEPLOY_DATABASE_URL}" >> "${file}"
  printf 'GOOGLE_CLIENT_ID=%s\n' "${DEPLOY_GOOGLE_CLIENT_ID}" >> "${file}"
  printf 'GOOGLE_CLIENT_SECRET=%s\n' "${DEPLOY_GOOGLE_CLIENT_SECRET}" >> "${file}"
  printf 'GOOGLE_REDIRECT_URI=%s\n' "${DEPLOY_GOOGLE_REDIRECT_URI}" >> "${file}"
  printf 'RESET_DB_ON_BOOT=false\n' >> "${file}"
  printf 'RUST_LOG=info\n' >> "${file}"
}

resolve_backend_bin() {
  local candidates=()
  local candidate

  if [[ -n "${CARGO_TARGET_DIR:-}" ]]; then
    if [[ "${CARGO_TARGET_DIR}" = /* ]]; then
      candidates+=("${CARGO_TARGET_DIR}/${DEPLOY_RUST_TARGET}/release/qstream-backend")
      candidates+=("${CARGO_TARGET_DIR}/release/qstream-backend")
    else
      candidates+=("${ROOT_DIR}/backend/${CARGO_TARGET_DIR}/${DEPLOY_RUST_TARGET}/release/qstream-backend")
      candidates+=("${ROOT_DIR}/backend/${CARGO_TARGET_DIR}/release/qstream-backend")
    fi
  fi

  candidates+=(
    "${ROOT_DIR}/backend/target/${DEPLOY_RUST_TARGET}/release/qstream-backend"
    "${ROOT_DIR}/backend/target-wsl/${DEPLOY_RUST_TARGET}/release/qstream-backend"
    "${ROOT_DIR}/backend/target-wsl/release/qstream-backend"
    "${ROOT_DIR}/backend/target/release/qstream-backend"
  )

  for candidate in "${candidates[@]}"; do
    if [[ -x "${candidate}" ]]; then
      printf '%s\n' "${candidate}"
      return 0
    fi
  done

  candidate="$(
    find "${ROOT_DIR}/backend" -maxdepth 5 -type f -name qstream-backend -path '*/release/*' -printf '%T@ %p\n' 2>/dev/null \
      | sort -nr \
      | head -n 1 \
      | cut -d' ' -f2-
  )"
  if [[ -n "${candidate}" && -x "${candidate}" ]]; then
    printf '%s\n' "${candidate}"
    return 0
  fi

  return 1
}

require_cmd ssh
require_cmd scp
require_cmd cargo
require_cmd npm
require_cmd tar

require_non_empty "${DEPLOY_SSH_TARGET}" "DEPLOY_SSH_TARGET"
require_non_empty "${DEPLOY_PUBLIC_HOST}" "DEPLOY_PUBLIC_HOST"
require_non_empty "${DEPLOY_SSH_KEY_PATH}" "DEPLOY_SSH_KEY_PATH"
require_non_empty "${DEPLOY_GOOGLE_CLIENT_ID}" "DEPLOY_GOOGLE_CLIENT_ID"
require_non_empty "${DEPLOY_GOOGLE_CLIENT_SECRET}" "DEPLOY_GOOGLE_CLIENT_SECRET"
require_non_empty "${DEPLOY_GOOGLE_REDIRECT_URI}" "DEPLOY_GOOGLE_REDIRECT_URI"
require_file "${PRODUCTION_CADDY_TEMPLATE}" "Production Caddy template"
require_file "${BACKEND_SERVICE_TEMPLATE}" "Backend systemd template"
require_file "${JOURNALD_TEMPLATE}" "Journald template"

assert_single_line "${DEPLOY_GOOGLE_CLIENT_ID}" "DEPLOY_GOOGLE_CLIENT_ID"
assert_single_line "${DEPLOY_GOOGLE_CLIENT_SECRET}" "DEPLOY_GOOGLE_CLIENT_SECRET"
assert_single_line "${DEPLOY_GOOGLE_REDIRECT_URI}" "DEPLOY_GOOGLE_REDIRECT_URI"

validate_port "${DEPLOY_BACKEND_PORT}" "DEPLOY_BACKEND_PORT"
[[ -f "${DEPLOY_SSH_KEY_PATH}" ]] || die "SSH key not found: ${DEPLOY_SSH_KEY_PATH}"

prepare_ssh_key

log "Checking server architecture..."
REMOTE_ARCH="$(
  ssh -i "${SSH_KEY_TEMP}" -o ConnectTimeout=15 -o StrictHostKeyChecking=accept-new "${DEPLOY_SSH_TARGET}" 'uname -m'
)"
TARGET_ARCH="$(target_arch_from_rust_target "${DEPLOY_RUST_TARGET}" || true)"
if [[ -n "${TARGET_ARCH}" && "${TARGET_ARCH}" != "${REMOTE_ARCH}" ]]; then
  die "Rust target (${DEPLOY_RUST_TARGET}, arch ${TARGET_ARCH}) differs from remote arch (${REMOTE_ARCH})."
fi

if [[ "${SKIP_BUILD}" != "1" ]]; then
  require_cmd rustup
  if [[ "${DEPLOY_RUST_TARGET}" == *-musl ]]; then
    require_cmd musl-gcc
  fi

  log "Ensuring Rust target is installed: ${DEPLOY_RUST_TARGET}"
  rustup target add "${DEPLOY_RUST_TARGET}" >/dev/null

  log "Building backend release binary for ${DEPLOY_RUST_TARGET}..."
  (cd backend && cargo build --release --target "${DEPLOY_RUST_TARGET}")

  if [[ "${INSTALL_FRONTEND_DEPS}" == "1" && ! -d frontend/node_modules ]]; then
    log "Installing frontend dependencies (npm ci)..."
    (cd frontend && npm ci)
  fi

  log "Building frontend static bundle..."
  (cd frontend && VITE_API_BASE_URL="${DEPLOY_PUBLIC_BASE_URL}" npm run build)
else
  log "Skipping local build because SKIP_BUILD=1"
fi

BACKEND_BIN="$(resolve_backend_bin || true)"
[[ -n "${BACKEND_BIN}" ]] || die "Backend binary not found. Expected release binary under backend/target*/${DEPLOY_RUST_TARGET}/release."
log "Using backend binary: ${BACKEND_BIN}"
[[ -d "${ROOT_DIR}/frontend/dist" ]] || die "Frontend dist not found: ${ROOT_DIR}/frontend/dist"

RELEASE_ID="$(date -u +%Y%m%d%H%M%S)"
BUNDLE_DIR="$(mktemp -d)"
BUNDLE_FILE="$(mktemp --suffix=.tar.gz)"

mkdir -p "${BUNDLE_DIR}/bin" "${BUNDLE_DIR}/frontend" "${BUNDLE_DIR}/config"
install -m 0755 "${BACKEND_BIN}" "${BUNDLE_DIR}/bin/qstream-backend"
cp -a "${ROOT_DIR}/frontend/dist/." "${BUNDLE_DIR}/frontend/"
write_backend_env_file "${BUNDLE_DIR}/backend.env"
render_template \
  "${BACKEND_SERVICE_TEMPLATE}" \
  "${BUNDLE_DIR}/config/qstream-backend.service" \
  "DEPLOY_SYSTEM_USER" "${DEPLOY_SYSTEM_USER}" \
  "DEPLOY_REMOTE_DIR" "${DEPLOY_REMOTE_DIR}"
render_template \
  "${PRODUCTION_CADDY_TEMPLATE}" \
  "${BUNDLE_DIR}/config/Caddyfile" \
  "DEPLOY_PUBLIC_HOST" "${DEPLOY_PUBLIC_HOST}" \
  "DEPLOY_BACKEND_PORT" "${DEPLOY_BACKEND_PORT}" \
  "DEPLOY_REMOTE_DIR" "${DEPLOY_REMOTE_DIR}"
install -m 0644 "${JOURNALD_TEMPLATE}" "${BUNDLE_DIR}/config/qstream-journald.conf"

tar -C "${BUNDLE_DIR}" -czf "${BUNDLE_FILE}" .

REMOTE_BUNDLE="/tmp/qstream-deploy-${RELEASE_ID}.tar.gz"

if [[ "${DRY_RUN}" == "1" ]]; then
  log "DRY_RUN=1; build/package checks passed, skipping upload/install."
  log "Selected backend binary: ${BACKEND_BIN}"
  log "Bundle file: ${BUNDLE_FILE}"
  exit 0
fi

log "Uploading release bundle to ${DEPLOY_SSH_TARGET}..."
scp -i "${SSH_KEY_TEMP}" -o ConnectTimeout=15 -o StrictHostKeyChecking=accept-new \
  "${BUNDLE_FILE}" "${DEPLOY_SSH_TARGET}:${REMOTE_BUNDLE}"

log "Installing release on remote server..."
ssh -i "${SSH_KEY_TEMP}" -o ConnectTimeout=15 -o StrictHostKeyChecking=accept-new \
  "${DEPLOY_SSH_TARGET}" \
  "DEPLOY_REMOTE_DIR='${DEPLOY_REMOTE_DIR}' DEPLOY_SYSTEM_USER='${DEPLOY_SYSTEM_USER}' DEPLOY_PUBLIC_HOST='${DEPLOY_PUBLIC_HOST}' DEPLOY_BACKEND_PORT='${DEPLOY_BACKEND_PORT}' REMOTE_BUNDLE='${REMOTE_BUNDLE}' RELEASE_ID='${RELEASE_ID}' bash -s" <<'REMOTE_SCRIPT'
set -Eeuo pipefail

if ! command -v sudo >/dev/null 2>&1; then
  echo "[deploy-remote] sudo is required on remote host" >&2
  exit 1
fi

if ! command -v caddy >/dev/null 2>&1; then
  sudo apt-get update
  sudo apt-get install -y debian-keyring debian-archive-keyring apt-transport-https curl gnupg
  curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' | sudo gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
  curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' | sudo tee /etc/apt/sources.list.d/caddy-stable.list >/dev/null
  sudo apt-get update
  sudo apt-get install -y caddy
fi

if ! id -u "${DEPLOY_SYSTEM_USER}" >/dev/null 2>&1; then
  sudo useradd --system --home-dir /var/lib/qstream --create-home --shell /usr/sbin/nologin "${DEPLOY_SYSTEM_USER}"
fi

free_backend_port() {
  local port="$1"
  local pids=""
  local listeners=""
  local attempt

  case "${port}" in
    22|80|443)
      echo "[deploy-remote] refusing to kill listeners on protected port ${port}" >&2
      return 1
      ;;
  esac

  for attempt in 1 2 3 4 5; do
    listeners="$(sudo ss -ltnp 2>/dev/null | awk -v port=":${port}" '$4 ~ port"$" {print}')"
    pids="$(printf '%s\n' "${listeners}" | sed -n 's/.*pid=\([0-9][0-9]*\),.*/\1/p' | sort -u)"

    if [[ -z "${pids}" ]] && command -v lsof >/dev/null 2>&1; then
      pids="$(sudo lsof -t -iTCP:${port} -sTCP:LISTEN 2>/dev/null | sort -u || true)"
    fi

    if [[ -z "${pids}" ]]; then
      return 0
    fi

    if printf '%s\n' "${listeners}" | grep -q 'node' \
      && (sudo systemctl is-active --quiet pm2-root 2>/dev/null || sudo systemctl is-enabled --quiet pm2-root 2>/dev/null); then
      echo "[deploy-remote] pm2-root owns backend port ${port}; stopping and disabling pm2-root"
      sudo systemctl stop pm2-root || true
      sudo systemctl disable pm2-root || true
    fi

    echo "[deploy-remote] killing listeners on port ${port}: ${pids}"
    sudo kill ${pids} || true
    sleep 1
    for pid in ${pids}; do
      if sudo kill -0 "${pid}" 2>/dev/null; then
        sudo kill -9 "${pid}" || true
      fi
    done
    sleep 1
  done

  echo "[deploy-remote] failed to free backend port ${port}" >&2
  sudo ss -ltnp | awk -v port=":${port}" '$4 ~ port"$" {print}' || true
  return 1
}

REMOTE_RELEASE_DIR="${DEPLOY_REMOTE_DIR}/releases/${RELEASE_ID}"
sudo mkdir -p "${DEPLOY_REMOTE_DIR}/releases" /etc/qstream /var/lib/qstream
sudo rm -rf "${REMOTE_RELEASE_DIR}"
sudo mkdir -p "${REMOTE_RELEASE_DIR}"
sudo tar -xzf "${REMOTE_BUNDLE}" -C "${REMOTE_RELEASE_DIR}"
sudo rm -f "${REMOTE_BUNDLE}"

sudo ln -sfn "${REMOTE_RELEASE_DIR}" "${DEPLOY_REMOTE_DIR}/current"
sudo chown -R root:root "${DEPLOY_REMOTE_DIR}"
# Ensure static files are world-readable so caddy user can serve frontend.
sudo chmod 755 "${DEPLOY_REMOTE_DIR}" "${DEPLOY_REMOTE_DIR}/releases" "${REMOTE_RELEASE_DIR}"
if [[ -d "${REMOTE_RELEASE_DIR}/frontend" ]]; then
  sudo find "${REMOTE_RELEASE_DIR}/frontend" -type d -exec chmod 755 {} +
  sudo find "${REMOTE_RELEASE_DIR}/frontend" -type f -exec chmod 644 {} +
fi
if [[ -f "${REMOTE_RELEASE_DIR}/bin/qstream-backend" ]]; then
  sudo chmod 755 "${REMOTE_RELEASE_DIR}/bin/qstream-backend"
fi
if [[ -f "${REMOTE_RELEASE_DIR}/backend.env" ]]; then
  sudo chmod 600 "${REMOTE_RELEASE_DIR}/backend.env"
fi
sudo chown -R "${DEPLOY_SYSTEM_USER}:${DEPLOY_SYSTEM_USER}" /var/lib/qstream

sudo install -m 0600 "${DEPLOY_REMOTE_DIR}/current/backend.env" /etc/qstream/backend.env
sudo chown root:root /etc/qstream/backend.env

sudo install -m 0644 "${DEPLOY_REMOTE_DIR}/current/config/qstream-backend.service" /etc/systemd/system/qstream-backend.service
sudo chmod 0644 /etc/systemd/system/qstream-backend.service

sudo install -m 0644 "${DEPLOY_REMOTE_DIR}/current/config/Caddyfile" /etc/caddy/Caddyfile
sudo chmod 0644 /etc/caddy/Caddyfile

sudo mkdir -p /etc/systemd/journald.conf.d /var/log/journal
sudo install -m 0644 "${DEPLOY_REMOTE_DIR}/current/config/qstream-journald.conf" /etc/systemd/journald.conf.d/qstream.conf
sudo chmod 0644 /etc/systemd/journald.conf.d/qstream.conf

sudo caddy fmt --overwrite /etc/caddy/Caddyfile
sudo caddy validate --config /etc/caddy/Caddyfile

# Caddy must own :80/:443 for ACME and HTTPS traffic.
if command -v nginx >/dev/null 2>&1; then
  if sudo systemctl is-enabled --quiet nginx 2>/dev/null || sudo systemctl is-active --quiet nginx 2>/dev/null; then
    sudo systemctl stop nginx || true
    sudo systemctl disable nginx || true
  fi
fi

sudo systemctl restart systemd-journald
sudo systemctl daemon-reload
sudo systemctl stop qstream-backend || true
free_backend_port "${DEPLOY_BACKEND_PORT}"
sudo systemctl enable qstream-backend
sudo systemctl restart qstream-backend
sudo systemctl enable caddy
sudo systemctl restart caddy

if ! sudo systemctl is-active --quiet qstream-backend; then
  echo "[deploy-remote] qstream-backend failed to start" >&2
  sudo journalctl -u qstream-backend -n 80 --no-pager >&2 || true
  exit 1
fi

sudo systemctl --no-pager --full status qstream-backend | sed -n '1,30p'
sudo systemctl --no-pager --full status caddy | sed -n '1,30p'
REMOTE_SCRIPT

log "Deployment finished."
log "App URL: https://${DEPLOY_PUBLIC_HOST}"
log "Backend logs: ssh ${DEPLOY_SSH_TARGET} 'sudo journalctl -u qstream-backend -f'"
log "Caddy logs: ssh ${DEPLOY_SSH_TARGET} 'sudo journalctl -u caddy -f'"
