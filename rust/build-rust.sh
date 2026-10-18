#!/usr/bin/env bash
#
# build-rust.sh — 一键编译本地 Rust 引擎（release profile，快速本地参数）
#
# 产物: rust/target/release/mirri
#
# 注意: 本脚本产出的是日常本地用的快速 release 构建（无全程序 LTO、
# codegen-units 默认）。发布打包走 rust/scripts/release-package.mjs，
# 它用 `--profile dist`（thin LTO + codegen-units=1）产出 target/dist/。
# 用法:
#   ./build-rust.sh               # 重建 web UI + 编译 release 产物并冒烟验证
#   ./build-rust.sh --no-smoke    # 跳过冒烟验证
#   ./build-rust.sh --no-stage-web # 跳过 web 重建与 staging，只编译
#   ./build-rust.sh <cargo 参数>  # 其余参数原样透传给 cargo build
#
# 可选环境变量:
#   CARGO_TARGET_DIR   覆写 cargo 输出目录（默认 <workspace>/target）
#
# 透传 --target <triple> / --target-dir <dir> 时产物路径随之修正
#（<target-dir>/<triple>/release/mirri）；交叉编译产物无法在宿主机运行，
# 冒烟验证自动跳过。
#
# 构建前先完整重建 web UI（pnpm -C apps/mirri-web run build），再把 dist
# staging 进 rust-embed 源目录（release 构建会把 web-dist 编进二进制）。
# --no-stage-web 跳过 web 重建与 staging 两个步骤，嵌入的仍是上次内容。

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

SMOKE=1
STAGE_WEB=1
CARGO_ARGS=()
TARGET_TRIPLE=""
TARGET_DIR_ARG=""
# 正在等待取值的透传 flag（--target / --target-dir 的空格分隔形式）
EXPECT=""

for arg in "$@"; do
  if [ -n "$EXPECT" ]; then
    # 上一个透传 flag 的取值：记录下来，同时照常透传给 cargo
    case "$EXPECT" in
      --target) TARGET_TRIPLE="$arg" ;;
      --target-dir) TARGET_DIR_ARG="$arg" ;;
    esac
    EXPECT=""
    CARGO_ARGS+=("$arg")
    continue
  fi
  case "$arg" in
    --no-smoke) SMOKE=0 ;;
    --no-stage-web) STAGE_WEB=0 ;;
    --target)
      EXPECT="--target"
      CARGO_ARGS+=("$arg")
      ;;
    --target=*)
      TARGET_TRIPLE="${arg#--target=}"
      CARGO_ARGS+=("$arg")
      ;;
    --target-dir)
      EXPECT="--target-dir"
      CARGO_ARGS+=("$arg")
      ;;
    --target-dir=*)
      TARGET_DIR_ARG="${arg#--target-dir=}"
      CARGO_ARGS+=("$arg")
      ;;
    *) CARGO_ARGS+=("$arg") ;;
  esac
done

TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
if [ -n "$TARGET_DIR_ARG" ]; then
  TARGET_DIR="$TARGET_DIR_ARG"
fi
if [ -n "$TARGET_TRIPLE" ]; then
  BIN="$TARGET_DIR/$TARGET_TRIPLE/release/mirri"
else
  BIN="$TARGET_DIR/release/mirri"
fi

cd "$ROOT"

# 自动重建并 staging web SPA：release 构建会把 crates/mirri-server/web-dist
# 编进二进制，先跑完整的 web 构建（而不是直接复用上一次的 dist），再 staging
# 保证嵌入的始终是最新构建。pnpm 不可用（或 apps/mirri-web 缺失）时跳过并
# 提示。
if [ "$STAGE_WEB" -eq 1 ]; then
  WEB_DIR="$ROOT/../apps/mirri-web"
  if command -v pnpm >/dev/null 2>&1 && [ -f "$WEB_DIR/package.json" ]; then
    echo "==> 完整重建 web SPA (pnpm -C apps/mirri-web run build)"
    pnpm -C "$WEB_DIR" run build
    echo "==> staging web SPA (apps/mirri-web/dist -> crates/mirri-server/web-dist)"
    node "$ROOT/scripts/stage-web.mjs"
  else
    echo "==> 跳过 web 重建与 staging: 将嵌入上次 staging 的内容" >&2
    echo "    （需要 pnpm 与 apps/mirri-web 可用）" >&2
  fi
fi

echo "==> cargo build --release (workspace: $ROOT)"
# bash 3.2 (macOS 默认) 在 set -u 下空数组展开会报 unbound variable，需分支调用
if [ "${#CARGO_ARGS[@]}" -eq 0 ]; then
  cargo build --release
else
  cargo build --release "${CARGO_ARGS[@]}"
fi

echo "==> 产物"
ls -lh "$BIN"
# `file` 在最小化 Linux 环境可能不存在：缺失时跳过，不能让成功的构建以失败告终
if command -v file >/dev/null 2>&1; then
  file "$BIN"
fi

# 交叉编译产物无法在宿主机执行，冒烟验证自动跳过
if [ "$SMOKE" -eq 1 ] && [ -n "$TARGET_TRIPLE" ]; then
  HOST_TRIPLE="$(rustc -vV | sed -n 's/^host: //p')"
  if [ "$TARGET_TRIPLE" != "$HOST_TRIPLE" ]; then
    echo "==> 跳过冒烟验证: 交叉编译产物 ($TARGET_TRIPLE) 无法在宿主机 ($HOST_TRIPLE) 上运行"
    SMOKE=0
  fi
fi

if [ "$SMOKE" -eq 1 ]; then
  echo "==> 冒烟验证"
  "$BIN" --version
  "$BIN" --help >/dev/null
  echo "==> 完成: 二进制可用 -> $BIN"
fi
