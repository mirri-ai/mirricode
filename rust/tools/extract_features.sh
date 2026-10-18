#!/bin/bash
# rust/tools/extract_features.sh
# 从 TS 代码库提取功能清单，建立 feature tracking matrix
# 用于跟踪 Rust 内核对 TS 功能的覆盖进度

set -e

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"

echo "=== Wire Op Types ==="
rg "type:" "$REPO_ROOT/packages/agent-core-v2/src/wire/" --type ts -o 2>/dev/null | sort -u || echo "(not found)"

echo ""
echo "=== Services ==="
rg "registerSingleton|registerTransient|registerScoped" "$REPO_ROOT/packages/agent-core-v2/src/services/" --type ts -l 2>/dev/null | wc -l || echo "0"

echo ""
echo "=== Tools ==="
rg -o '"\w+"' "$REPO_ROOT/packages/agent-core/src/tools/" --type ts -g "*.ts" 2>/dev/null | sort -u || echo "(not found)"

echo ""
echo "=== Events ==="
rg "type:" "$REPO_ROOT/packages/agent-core/src/events/" --type ts -o 2>/dev/null | sort -u || echo "(not found)"

echo ""
echo "=== Routes ==="
rg "router\.(get|post|put|delete|patch)" "$REPO_ROOT/packages/agent-core-v2/src/" --type ts -o 2>/dev/null | sort -u || echo "(not found)"
