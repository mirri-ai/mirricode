#!/usr/bin/env bash
# Replaces the installed mirri CLI with the native build output and re-signs
# it so macOS does not SIGKILL the binary on launch.
#
# On Apple Silicon (and macOS 15+ Sequoia / Tahoe), overwriting a binary that
# carries a stale `com.apple.provenance` xattr with a new build causes the
# kernel to kill it at exec — `zsh: killed` / exit 137 — even though the code
# signature itself is valid. Clearing the xattr and re-signing ad-hoc fixes it.
#
# Usage:
#   ./scripts/replace_on_mac.sh                # auto-detect arch, default home
#   MIRRICODE_HOME=/tmp/mirri ./scripts/replace_on_mac.sh
#   MIRRI_CODE_BUILD_TARGET=darwin-x64 ./scripts/replace_on_mac.sh
set -euo pipefail

# --- locate build output ------------------------------------------------------
if [[ "$(uname)" != "Darwin" ]]; then
  echo "replace_on_mac.sh only runs on macOS." >&2
  exit 1
fi

arch="$(uname -m)"
# paths.mjs targetTriple(): MIRRI_CODE_BUILD_TARGET wins, else ${platform}-${arch}
# target="${MIRRI_CODE_BUILD_TARGET:-darwin-${arch}}"
target="$(node -e "process.stdout.write(process.platform + '-' + process.arch)")"

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# scripts/ -> apps/mirri-code/
app_root="$(cd "${script_dir}/.." && pwd)"
src="${app_root}/dist-native/bin/${target}/mirri"

if [[ ! -f "${src}" ]]; then
  echo "Build output not found: ${src}" >&2
  echo "Run \`pnpm --filter @mirri-ai/mirri-code run build:native:sea\` first." >&2
  exit 1
fi

# --- locate install dir -------------------------------------------------------
home_dir="${MIRRICODE_HOME:-$HOME/.mirri-code}"
dst_dir="${home_dir}/bin"
dst="${dst_dir}/mirri"

mkdir -p "${dst_dir}"

# --- backup existing binary ---------------------------------------------------
if [[ -f "${dst}" ]]; then
  stamp="$(date +%Y%m%d-%H%M%S)"
  backup="${dst}.bak.${stamp}"
  cp "${dst}" "${backup}"
  echo "Backed up existing binary to ${backup}"
fi

# --- replace + fix macOS provenance / signing ---------------------------------
cp "${src}" "${dst}"
chmod 755 "${dst}"

# Drop stale provenance + any quarantine attributes from the previous binary.
xattr -cr "${dst}"

# Re-sign ad-hoc so the signature matches the new bytes on disk. Matches the
# local profile in scripts/native/04-sign.mjs (identity "-").
codesign --sign - --force "${dst}" >/dev/null 2>&1

# --- verify -------------------------------------------------------------------
# Capture output first — `grep -q` under `pipefail` would close the pipe early
# (SIGPIPE to codesign) and falsely fail the pipeline.
verify_out="$(codesign --verify --verbose=2 "${dst}" 2>&1)"
if ! grep -q "valid on disk" <<<"${verify_out}"; then
  echo "Signature verification failed for ${dst}" >&2
  printf '%s\n' "${verify_out}" >&2
  exit 1
fi

# --- smoke test ----------------------------------------------------------------
if ! version_out="$("${dst}" --version 2>&1)"; then
  echo "Smoke test failed: \`mirri --version\` did not run." >&2
  printf '%s\n' "${version_out}" >&2
  exit 1
fi

echo "Replaced ${dst}"
echo "  source : ${src}"
echo "  target : ${target}"
echo "  version: ${version_out}"
echo "  signed : ad-hoc (valid on disk)"
