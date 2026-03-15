#!/usr/bin/env bash
set -euo pipefail

require_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "Missing required command: $1" >&2
    exit 1
  fi
}

if [[ "${EUID}" -ne 0 ]]; then
  echo "Run this updater as root." >&2
  exit 1
fi

require_cmd curl
require_cmd python3
require_cmd sha256sum
require_cmd tar
require_cmd install
require_cmd systemctl

REPO="${GH_RELEASE_REPO:-antimony-labs/core}"
BINARY_NAME="${CORE_API_BINARY_NAME:-fleet_api}"
ASSET_NAME="${CORE_API_RELEASE_ASSET:-fleet_api-x86_64-unknown-linux-gnu.tar.gz}"
CHECKSUM_NAME="${ASSET_NAME}.sha256"
INSTALL_PATH="${CORE_API_INSTALL_PATH:-/usr/local/bin/${BINARY_NAME}}"
SERVICE_NAME="${CORE_API_SERVICE_NAME:-fleet-api.service}"
STATE_DIR="${CORE_API_STATE_DIR:-/var/lib/antimony/core-api}"
STATE_FILE="${STATE_DIR}/current-release"

api_headers=(
  -H "Accept: application/vnd.github+json"
  -H "X-GitHub-Api-Version: 2022-11-28"
)
asset_headers=(
  -H "Accept: application/octet-stream"
  -H "X-GitHub-Api-Version: 2022-11-28"
)

if [[ -n "${GH_RELEASE_TOKEN:-}" ]]; then
  api_headers+=(-H "Authorization: Bearer ${GH_RELEASE_TOKEN}")
  asset_headers+=(-H "Authorization: Bearer ${GH_RELEASE_TOKEN}")
fi

tmpdir="$(mktemp -d)"
trap 'rm -rf "${tmpdir}"' EXIT

release_json="$(curl -fsSL "${api_headers[@]}" "https://api.github.com/repos/${REPO}/releases/latest")"

mapfile -t release_meta < <(
  RELEASE_JSON="${release_json}" python3 - "${ASSET_NAME}" "${CHECKSUM_NAME}" <<'PY'
import json
import os
import sys

asset_name = sys.argv[1]
checksum_name = sys.argv[2]
release = json.loads(os.environ["RELEASE_JSON"])
assets = {asset["name"]: asset["url"] for asset in release.get("assets", [])}

if asset_name not in assets:
    raise SystemExit(f"Release asset not found: {asset_name}")
if checksum_name not in assets:
    raise SystemExit(f"Checksum asset not found: {checksum_name}")

print(release["tag_name"])
print(assets[asset_name])
print(assets[checksum_name])
PY
)

release_tag="${release_meta[0]}"
asset_url="${release_meta[1]}"
checksum_url="${release_meta[2]}"

mkdir -p "${STATE_DIR}"
if [[ -f "${STATE_FILE}" ]] && [[ "$(cat "${STATE_FILE}")" == "${release_tag}" ]]; then
  echo "Core API already on ${release_tag}."
  exit 0
fi

curl -fsSL "${asset_headers[@]}" "${asset_url}" -o "${tmpdir}/${ASSET_NAME}"
curl -fsSL "${asset_headers[@]}" "${checksum_url}" -o "${tmpdir}/${CHECKSUM_NAME}"

expected_sha="$(awk '{print $1}' "${tmpdir}/${CHECKSUM_NAME}")"
actual_sha="$(sha256sum "${tmpdir}/${ASSET_NAME}" | awk '{print $1}')"
if [[ "${expected_sha}" != "${actual_sha}" ]]; then
  echo "Checksum mismatch for ${ASSET_NAME}." >&2
  exit 1
fi

release_dir="${STATE_DIR}/releases/${release_tag}"
mkdir -p "${release_dir}"
tar -xzf "${tmpdir}/${ASSET_NAME}" -C "${release_dir}"

candidate="${release_dir}/${BINARY_NAME}"
if [[ ! -x "${candidate}" ]]; then
  echo "Release asset did not contain ${BINARY_NAME}." >&2
  exit 1
fi

backup_path=""
if [[ -f "${INSTALL_PATH}" ]]; then
  backup_path="${tmpdir}/${BINARY_NAME}.bak"
  cp "${INSTALL_PATH}" "${backup_path}"
fi

install -m 0755 "${candidate}" "${INSTALL_PATH}.new"
mv "${INSTALL_PATH}.new" "${INSTALL_PATH}"

if ! systemctl restart "${SERVICE_NAME}"; then
  if [[ -n "${backup_path}" ]]; then
    install -m 0755 "${backup_path}" "${INSTALL_PATH}.new"
    mv "${INSTALL_PATH}.new" "${INSTALL_PATH}"
    systemctl restart "${SERVICE_NAME}" || true
  fi
  echo "Failed to restart ${SERVICE_NAME}; rolled binary back." >&2
  exit 1
fi

if ! systemctl is-active --quiet "${SERVICE_NAME}"; then
  if [[ -n "${backup_path}" ]]; then
    install -m 0755 "${backup_path}" "${INSTALL_PATH}.new"
    mv "${INSTALL_PATH}.new" "${INSTALL_PATH}"
    systemctl restart "${SERVICE_NAME}" || true
  fi
  echo "${SERVICE_NAME} did not become healthy after restart." >&2
  exit 1
fi

printf '%s\n' "${release_tag}" > "${STATE_FILE}"
echo "Updated Core API to ${release_tag}."
