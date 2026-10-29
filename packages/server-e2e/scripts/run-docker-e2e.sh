#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
PACKAGE_DIR="$(cd -- "${SCRIPT_DIR}/.." && pwd)"
REPO_ROOT="$(cd -- "${PACKAGE_DIR}/../.." && pwd)"

workspace_slug="$(
  basename -- "${REPO_ROOT}" \
    | tr '[:upper:]' '[:lower:]' \
    | tr -cs 'a-z0-9_.-' '-' \
    | sed -e 's/^[^a-z0-9]*//' -e 's/[^a-z0-9]*$//' \
    | cut -c1-48
)"
if [[ -z "${workspace_slug}" ]]; then
  workspace_slug="workspace"
fi
workspace_hash="$(printf '%s' "${REPO_ROOT}" | cksum | awk '{print $1}')"
RUN_ID="${MIRRICODE_SERVER_E2E_RUN_ID:-${workspace_slug}-${workspace_hash}}"

BASE_IMAGE="${MIRRICODE_SERVER_E2E_BASE_IMAGE:-mirri-server-e2e-base:${RUN_ID}}"
IMAGE="${MIRRICODE_SERVER_E2E_IMAGE:-mirri-server-e2e:${RUN_ID}}"
CONTAINER="${MIRRICODE_SERVER_E2E_CONTAINER:-mirri-server-e2e-${RUN_ID}}"
STATE_ROOT="${MIRRICODE_SERVER_E2E_STATE_ROOT:-${HOME}/.mirri-code-server-dev}"

if [[ -n "${MIRRICODE_SERVER_E2E_REPORT_DIR_HOST:-}" ]]; then
  REPORT_DIR_HOST="${MIRRICODE_SERVER_E2E_REPORT_DIR_HOST}"
  REPORT_ROOT_HOST="$(dirname -- "${REPORT_DIR_HOST}")"
  REPORT_DIR_NAME="$(basename -- "${REPORT_DIR_HOST}")"
else
  REPORT_ROOT_HOST="${MIRRICODE_SERVER_E2E_REPORT_ROOT_HOST:-${STATE_ROOT}/server-e2e-reports/docker/${RUN_ID}}"
  REPORT_DIR_NAME="latest"
  REPORT_DIR_HOST="${REPORT_ROOT_HOST}/${REPORT_DIR_NAME}"
fi
REPORT_ROOT_CONTAINER="/data/server-e2e-reports/docker"
REPORT_DIR_CONTAINER="${REPORT_ROOT_CONTAINER}/${REPORT_DIR_NAME}"
TMPDIR_CONTAINER="/data/docker-e2e/tmp"

NM_ROOT="${STATE_ROOT}/docker-e2e/${RUN_ID}/nm"

workspace_node_modules=(
  "root:/workspace/mirricode/node_modules"
  "apps_mirri-web:/workspace/mirricode/apps/mirri-web/node_modules"
  "docs:/workspace/mirricode/docs/node_modules"
  "pkg_server-e2e:/workspace/mirricode/packages/server-e2e/node_modules"
  "pkg_protocol:/workspace/mirricode/packages/protocol/node_modules"
  "pkg_e2e-harness:/workspace/mirricode/packages/e2e-harness/node_modules"
  "pkg_release:/workspace/mirricode/packages/release/node_modules"
  "apps_mirri-desktop:/workspace/mirricode/apps/mirri-desktop/node_modules"
)

mkdir -p "${STATE_ROOT}" "${REPORT_DIR_HOST}" "${NM_ROOT}"
for mount in "${workspace_node_modules[@]}"; do
  mkdir -p "${NM_ROOT}/${mount%%:*}"
done

# Journey scenarios are self-contained: each one boots its own serve processes
# with isolated temp homes through the orchestrator (src/proc.ts) and its own
# in-process fake provider (@mirri-ai/e2e-harness), so the docker flow only has
# to provide a built rust binary and the workspace node dependencies — no
# pre-started server, healthz gate, or seeded config.toml on the host side.

if [[ "${MIRRICODE_SERVER_E2E_SKIP_BUILD:-0}" != "1" ]]; then
  docker build -t "${BASE_IMAGE}" -f "${REPO_ROOT}/Dockerfile" "${REPO_ROOT}"
  docker build \
    -t "${IMAGE}" \
    -f "${PACKAGE_DIR}/Dockerfile" \
    --build-arg "BASE_IMAGE=${BASE_IMAGE}" \
    "${REPO_ROOT}"
fi

docker rm -f "${CONTAINER}" >/dev/null 2>&1 || true

read -r -d '' container_script <<'EOS' || true
set -euo pipefail

# bash -lc resets PATH; re-export the toolchain paths baked into the image.
export PATH="/usr/local/cargo/bin:/root/.local/share/pnpm:/usr/local/bin:${PATH}"

cd /workspace/mirricode
mkdir -p "${MIRRICODE_SERVER_E2E_REPORT_DIR}" "${TMPDIR}" /data/server-e2e-reports/docker

if [[ ! -e /workspace/mirricode/node_modules/.modules.yaml || ! -e /workspace/mirricode/packages/server-e2e/node_modules/ws ]]; then
  echo "[server-e2e:docker] installing pnpm deps"
  pnpm install --frozen-lockfile
else
  echo "[server-e2e:docker] pnpm deps already present"
fi

# Build into the writable docker volume layer (the repo is mounted read-only).
echo "[server-e2e:docker] building rust binary"
export CARGO_TARGET_DIR="/data/rust-target"
cargo build -p mirri-cli --manifest-path rust/Cargo.toml --locked \
  --target-dir "${CARGO_TARGET_DIR}"

# Each journey scenario boots its own serve processes with isolated temp homes
# through the orchestrator (src/proc.ts); point its binary resolution at the
# container-built artifact instead of the read-only repo target dir.
export MIRRICODE_E2E_RUST_BIN="${CARGO_TARGET_DIR}/debug/mirri"

cd /workspace/mirricode/packages/server-e2e
pnpm test:scenarios
EOS

docker_args=(
  run
  --rm
  --init
  --name "${CONTAINER}"
  --workdir /workspace/mirricode/packages/server-e2e
  --env "MIRRICODE_SERVER_E2E_REPORT_DIR=${REPORT_DIR_CONTAINER}"
  --env "TMPDIR=${TMPDIR_CONTAINER}"
  --env "TERM=xterm-256color"
  --env "TZ=Asia/Shanghai"
  --env "npm_config_store_dir=/workspace/mirricode/node_modules/.pnpm-store"
  --env "npm_config_package_import_method=copy"
  --volume "${REPO_ROOT}:/workspace/mirricode:ro"
  --volume "${REPORT_ROOT_HOST}:${REPORT_ROOT_CONTAINER}"
)

for mount in "${workspace_node_modules[@]}"; do
  docker_args+=(--volume "${NM_ROOT}/${mount%%:*}:${mount#*:}")
done

echo "[server-e2e:docker] running ${IMAGE} without host port publishing"
set +e
docker "${docker_args[@]}" "${IMAGE}" bash -lc "${container_script}"
status=$?
set -e

echo "[server-e2e:docker] report: ${REPORT_DIR_HOST}/index.html"
exit "${status}"
