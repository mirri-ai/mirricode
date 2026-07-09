#!/usr/bin/env bash
#
# mirri-code installer for macOS and Linux.
#
# Usage:
#   curl -fsSL https://mirricode.com/install.sh | bash
#   curl -fsSL https://mirricode.com/install.sh | MIRRICODE_VERSION=0.5.0 bash
#   curl -fsSL https://mirricode.com/install.sh | bash -s -- --version 0.5.0
#   curl -fsSL https://mirricode.com/install.sh | sudo env MIRRICODE_INSTALL_DIR=/usr/local bash
#
# Optional env:
#   MIRRICODE_VERSION         Explicit version; if unset, fetch latest from GitHub
#   MIRRICODE_INSTALL_DIR     Installation directory, default $HOME/.mirri-code
#   MIRRICODE_NO_MODIFY_PATH  Skip PATH modification when set to a non-empty value
#
# Optional args:
#   --version VERSION    Explicit version; equivalent to MIRRICODE_VERSION=VERSION

set -euo pipefail

GITHUB_REPO="mirri-ai/mirricode"
GITHUB_API="https://api.github.com/repos/${GITHUB_REPO}"
GITHUB_RELEASES="https://github.com/${GITHUB_REPO}/releases/download"
NPM_PACKAGE="@mirri-ai/mirri-code"
TAG_PREFIX="${NPM_PACKAGE}@"

MIRRICODE_VERSION="${MIRRICODE_VERSION:-}"
MIRRICODE_INSTALL_DIR="${MIRRICODE_INSTALL_DIR:-$HOME/.mirri-code}"
MIRRICODE_NO_MODIFY_PATH="${MIRRICODE_NO_MODIFY_PATH:-}"

MIRRICODE_PATH_UPDATED_RC=""

# ---------- helpers ----------

_have() { command -v "$1" >/dev/null 2>&1; }

_log() {
  if [ -t 1 ]; then
    printf '\033[1;36m==>\033[0m %s\n' "$*"
  else
    printf '==> %s\n' "$*"
  fi
}

_err() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

_usage() {
  cat <<'EOF'
Usage:
  curl -fsSL https://mirricode.com/install.sh | bash
  curl -fsSL https://mirricode.com/install.sh | MIRRICODE_VERSION=0.5.0 bash
  curl -fsSL https://mirricode.com/install.sh | bash -s -- --version 0.5.0
  curl -fsSL https://mirricode.com/install.sh | sudo env MIRRICODE_INSTALL_DIR=/usr/local bash

Options:
  --version VERSION    Install a specific mirri-code version
  -h, --help           Show this help

Environment:
  MIRRICODE_VERSION         Explicit version; if unset, fetch latest
  MIRRICODE_INSTALL_DIR     Installation directory, default $HOME/.mirri-code
  MIRRICODE_NO_MODIFY_PATH  Skip PATH modification when set
EOF
}

_parse_args() {
  while [ $# -gt 0 ]; do
    case "$1" in
      -h|--help)
        _usage
        exit 0
        ;;
      --version)
        [ -n "${2:-}" ] || _err "--version requires a value"
        MIRRICODE_VERSION="$2"
        shift 2
        ;;
      --version=*)
        MIRRICODE_VERSION="${1#--version=}"
        [ -n "$MIRRICODE_VERSION" ] || _err "--version requires a value"
        shift
        ;;
      -*)
        _err "unknown option: $1"
        ;;
      *)
        [ -z "$MIRRICODE_VERSION" ] || _err "unexpected extra argument: $1"
        MIRRICODE_VERSION="$1"
        shift
        ;;
    esac
  done
}

_detect_target() {
  local os arch
  case "$(uname -s)" in
    Darwin) os="darwin" ;;
    Linux)  os="linux"  ;;
    MINGW*|MSYS*|CYGWIN*)
      _err "Windows is not supported by install.sh — use install.ps1 (PowerShell)"
      ;;
    *) _err "unsupported OS: $(uname -s)" ;;
  esac
  case "$(uname -m)" in
    x86_64|amd64) arch="x64" ;;
    arm64|aarch64) arch="arm64" ;;
    *) _err "unsupported architecture: $(uname -m)" ;;
  esac

  # Rosetta 2: when running under an x64 shell on an ARM Mac, switch back to the native arm64 binary
  if [ "$os" = "darwin" ] && [ "$arch" = "x64" ]; then
    if [ "$(sysctl -n sysctl.proc_translated 2>/dev/null)" = "1" ]; then
      arch="arm64"
    fi
  fi

  # musl detection (Alpine, etc.) — we currently only ship glibc binaries, so failing early is better than a runtime dlopen error
  if [ "$os" = "linux" ]; then
    if [ -f "/lib/libc.musl-x86_64.so.1" ] || \
       [ -f "/lib/libc.musl-aarch64.so.1" ] || \
       ldd /bin/ls 2>&1 | grep -q musl; then
      _err "Alpine / musl Linux is not currently supported. Use Node.js + 'npm install -g ${NPM_PACKAGE}' instead."
    fi
  fi

  echo "${os}-${arch}"
}

_download() {
  local url="$1" dest="${2:-}"
  if _have curl; then
    if [ -n "$dest" ]; then
      if [ -t 1 ]; then
        curl --fail --location --progress-bar -o "$dest" "$url"
      else
        curl --fail --location --silent -o "$dest" "$url"
      fi
    else
      curl --fail --location --silent "$url"
    fi
  elif _have wget; then
    if [ -n "$dest" ]; then
      wget -q -O "$dest" "$url"
    else
      wget -q -O - "$url"
    fi
  else
    _err "curl or wget is required"
  fi
}

# Prefer jq; otherwise parse a single manifest field using pure bash regex
_manifest_field() {
  local manifest_json="$1" target="$2" field="$3"
  if _have jq; then
    printf '%s' "$manifest_json" | jq -er ".platforms[\"$target\"].$field // empty"
  else
    local one_line
    one_line="$(printf '%s' "$manifest_json" | tr -d '\n\r\t' | sed 's/ \+/ /g')"
    if [[ $one_line =~ \"$target\"[^}]*\"$field\"[[:space:]]*:[[:space:]]*\"([^\"]+)\" ]]; then
      printf '%s' "${BASH_REMATCH[1]}"
    fi
  fi
}

_sha256_check() {
  local file="$1" expected="$2"
  local actual
  if _have shasum; then
    actual="$(shasum -a 256 "$file" | cut -d' ' -f1)"
  elif _have sha256sum; then
    actual="$(sha256sum "$file" | cut -d' ' -f1)"
  else
    _err "shasum or sha256sum required to verify download"
  fi
  if [ "$actual" != "$expected" ]; then
    _err "checksum mismatch: expected $expected, got $actual"
  fi
}

_detect_shell_rc() {
  local shell_name
  shell_name="$(basename "${SHELL:-/bin/bash}")"
  case "$shell_name" in
    zsh)  echo "$HOME/.zshrc" ;;
    bash)
      if [ -f "$HOME/.bashrc" ]; then echo "$HOME/.bashrc"
      elif [ -f "$HOME/.bash_profile" ]; then echo "$HOME/.bash_profile"
      elif [ -f "$HOME/.profile" ]; then echo "$HOME/.profile"
      else echo "$HOME/.bashrc"; fi
      ;;
    fish) echo "$HOME/.config/fish/config.fish" ;;
    *)    echo "$HOME/.profile" ;;
  esac
}

_update_path() {
  if [ -n "$MIRRICODE_NO_MODIFY_PATH" ]; then
    _log "Skipping PATH update (MIRRICODE_NO_MODIFY_PATH set)"
    return
  fi
  case ":$PATH:" in
    *":${MIRRICODE_INSTALL_DIR}/bin:"*)
      _log "${MIRRICODE_INSTALL_DIR}/bin already in PATH"
      return
      ;;
  esac
  local rc
  rc="$(_detect_shell_rc)"
  mkdir -p "$(dirname "$rc")"
  local export_line
  if [[ "$rc" == *fish* ]]; then
    export_line="fish_add_path -g \"${MIRRICODE_INSTALL_DIR}/bin\""
  else
    export_line="export PATH=\"${MIRRICODE_INSTALL_DIR}/bin:\$PATH\""
  fi
  if ! grep -qsF "${MIRRICODE_INSTALL_DIR}/bin" "$rc"; then
    printf '\n# mirri-code\n%s\n' "$export_line" >> "$rc"
    _log "Added ${MIRRICODE_INSTALL_DIR}/bin to PATH in $rc"
  else
    _log "${MIRRICODE_INSTALL_DIR}/bin already configured in $rc"
  fi
  MIRRICODE_PATH_UPDATED_RC="$rc"
}

# ---------- main ----------

TMPDIR_INSTALL=""
_cleanup() {
  if [ -n "$TMPDIR_INSTALL" ] && [ -d "$TMPDIR_INSTALL" ]; then
    rm -rf "$TMPDIR_INSTALL"
  fi
}
trap _cleanup EXIT

main() {
  local target version tag manifest_url manifest filename checksum binary_url tmpdir

  _parse_args "$@"

  target="$(_detect_target)"
  _log "Detected target: $target"

  # 1. Resolve version
  if [ -n "$MIRRICODE_VERSION" ]; then
    version="$MIRRICODE_VERSION"
    _log "Using pinned version $version"
  else
    _log "Resolving latest version from GitHub"
    local api_response
    api_response="$(_download "${GITHUB_API}/releases/latest")"
    tag="$(printf '%s' "$api_response" | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -1)"
    [ -n "$tag" ] || _err "could not resolve latest release from GitHub"
    # Strip the @mirri-ai/mirri-code@ prefix to get the raw semver
    version="${tag#"${TAG_PREFIX}"}"
    if [ "$version" = "$tag" ]; then
      # Fallback: strip v prefix if tag doesn't use the expected prefix
      version="${tag#v}"
    fi
    [ -n "$version" ] || _err "could not parse version from tag: $tag"
    _log "Latest version: $version"
  fi

  tag="${TAG_PREFIX}${version}"
  manifest_url="${GITHUB_RELEASES}/${tag}/manifest.json"
  _log "Fetching manifest ${manifest_url}"
  manifest="$(_download "$manifest_url")"
  [ -n "$manifest" ] || _err "manifest is empty or unreachable: $manifest_url"

  # 3. Find current platform entry
  filename="$(_manifest_field "$manifest" "$target" "filename")"
  checksum="$(_manifest_field "$manifest" "$target" "checksum")"
  [ -n "$filename" ] || _err "platform $target not found in manifest"
  [[ "$checksum" =~ ^[a-f0-9]{64}$ ]] || _err "invalid checksum for $target: $checksum"

  # 4. Download binary
  TMPDIR_INSTALL="$(mktemp -d 2>/dev/null || mktemp -d -t mirri-install)"
  tmpdir="$TMPDIR_INSTALL"
  binary_url="${GITHUB_RELEASES}/${tag}/${filename}"
  _log "Downloading ${binary_url}"
  _download "$binary_url" "${tmpdir}/${filename}"

  # 5. Verify checksum
  _log "Verifying checksum"
  _sha256_check "${tmpdir}/${filename}" "$checksum"

  # 6. Install
  chmod +x "${tmpdir}/${filename}"
  mkdir -p "${MIRRICODE_INSTALL_DIR}/bin"
  if [ -f "${MIRRICODE_INSTALL_DIR}/bin/mirri" ]; then
    cp "${MIRRICODE_INSTALL_DIR}/bin/mirri" "${MIRRICODE_INSTALL_DIR}/bin/mirri.bak"
    _log "Backed up existing mirri to ${MIRRICODE_INSTALL_DIR}/bin/mirri.bak"
  fi
  install -m 0755 "${tmpdir}/${filename}" "${MIRRICODE_INSTALL_DIR}/bin/mirri"
  _log "Installed to ${MIRRICODE_INSTALL_DIR}/bin/mirri"

  # 7. PATH
  _update_path

  _log "Done. Run: mirri --version"

  if [ -n "$MIRRICODE_PATH_UPDATED_RC" ]; then
    if [ -t 1 ]; then
      printf '\033[1;33m==> If `mirri` is not found, restart your shell or run:\033[0m \033[1;4msource %s\033[0m\n' "$MIRRICODE_PATH_UPDATED_RC"
    else
      printf '==> If `mirri` is not found, restart your shell or run: source %s\n' "$MIRRICODE_PATH_UPDATED_RC"
    fi
  fi
}

main "$@"
