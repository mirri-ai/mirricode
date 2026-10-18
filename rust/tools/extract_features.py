#!/usr/bin/env python3
"""Extract a structured feature tracking matrix from the Rust kernel.

Outputs JSON to stdout (or a file with --output) with four sections:
  - wire_ops: every WireOp variant -> whether it appears in the reducer
  - builtin_tools: every name in BUILTIN_TOOL_NAMES -> whether the name
    appears in the tools module (heuristic, not a full impl check)
  - rpc_methods: every method string in rpc.rs -> the handler struct name
  - events: every event variant in events.rs

Usage:
    python3 rust/tools/extract_features.py [--output path]
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent.parent
RUST_DIR = REPO_ROOT / "rust" / "crates"


def _extract_enum_variants(source: str, enum_name: str) -> list[str]:
    """Extract variant names from a Rust enum body, scoping to the enum
    definition to avoid matching serde attribute tokens like `default`.
    Uses brace-depth counting to handle nested struct fields.
    """
    # Find the enum start
    pat = rf"pub enum {enum_name}\s*\{{"
    m = re.search(pat, source)
    if not m:
        return []
    # Walk from the opening brace, tracking depth
    start = m.end()
    depth = 1
    i = start
    while i < len(source) and depth > 0:
        if source[i] == "{":
            depth += 1
        elif source[i] == "}":
            depth -= 1
        i += 1
    body = source[start : i - 1]
    # Remove all attribute blocks `#[...]` (including multi-line) to avoid
    # matching `default,` inside `#[serde(default, ...)]` blocks
    body = re.sub(r"#\[[^\]]*\]", "", body, flags=re.DOTALL)
    # Struct variants: `Variant {`
    struct_variants = re.findall(r"^\s+(\w+)\s*\{", body, re.MULTILINE)
    # Unit variants: `Variant,`
    unit_variants = re.findall(r"^\s+(\w+),", body, re.MULTILINE)
    return sorted(set(struct_variants + unit_variants) - {"Unknown"})


def extract_wire_ops() -> list[dict]:
    """Extract all WireOp variants and check reducer coverage."""
    wire_path = RUST_DIR / "mirri-proto" / "src" / "wire.rs"
    reducer_path = RUST_DIR / "mirri-proto" / "src" / "reducer.rs"

    if not wire_path.exists():
        return []

    source = wire_path.read_text(errors="replace")
    all_variants = _extract_enum_variants(source, "WireOp")

    reducer_source = ""
    if reducer_path.exists():
        reducer_source = reducer_path.read_text(errors="replace")

    results = []
    for v in all_variants:
        # Check if the variant appears in the reducer's match arms
        in_reducer = v in reducer_source
        results.append({"name": v, "in_reducer": in_reducer})

    return results


def extract_builtin_tools() -> list[dict]:
    """Extract BUILTIN_TOOL_NAMES and check for name presence in the tools
    module. This is a heuristic — it checks whether the name string appears
    in tools/mod.rs, not whether a full Tool impl exists in a per-tool file.
    """
    tools_mod_path = RUST_DIR / "mirri-core" / "src" / "tools" / "mod.rs"

    if not tools_mod_path.exists():
        return []

    source = tools_mod_path.read_text(errors="replace")

    # Extract names from BUILTIN_TOOL_NAMES
    names_match = re.search(r"BUILTIN_TOOL_NAMES[^=]*=\s*&?\[([^\]]+)\]", source)
    if not names_match:
        return []

    names_str = names_match.group(1)
    names = re.findall(r'"([^"]+)"', names_str)

    results = []
    for name in names:
        found = name in source
        results.append({"name": name, "has_impl": found})

    return results


def extract_rpc_methods() -> list[dict]:
    """Extract RPC method strings and pair with handler struct names.
    Returns an enumeration (not a coverage check) — all methods have
    handlers by construction.
    """
    rpc_path = RUST_DIR / "mirri-server" / "src" / "rpc.rs"

    if not rpc_path.exists():
        return []

    source = rpc_path.read_text(errors="replace")

    # Find all method() returns
    methods = re.findall(r'fn method\(&self\) -> &\'static str \{\s*"([^"]+)"', source)

    # Find all struct names that implement RpcHandler
    structs = re.findall(r"pub struct (\w+Handler)", source)

    results = []
    for i, method in enumerate(methods):
        handler_name = structs[i] if i < len(structs) else "unknown"
        results.append({"method": method, "handler": handler_name})

    return results


def extract_events() -> list[dict]:
    """Extract event variants from events.rs."""
    events_path = RUST_DIR / "mirri-proto" / "src" / "events.rs"

    if not events_path.exists():
        return []

    source = events_path.read_text(errors="replace")
    all_variants = _extract_enum_variants(source, "DomainEvent")

    return [{"name": v} for v in all_variants]


def main():
    parser = argparse.ArgumentParser(description="Extract Rust feature tracking matrix")
    parser.add_argument("--output", "-o", type=Path, help="Output file (default: stdout)")
    args = parser.parse_args()

    matrix = {
        "wire_ops": extract_wire_ops(),
        "builtin_tools": extract_builtin_tools(),
        "rpc_methods": extract_rpc_methods(),
        "events": extract_events(),
    }

    # Compute summary stats
    wire_total = len(matrix["wire_ops"])
    wire_covered = sum(1 for w in matrix["wire_ops"] if w["in_reducer"])
    tools_total = len(matrix["builtin_tools"])
    tools_covered = sum(1 for t in matrix["builtin_tools"] if t["has_impl"])

    matrix["summary"] = {
        "wire_ops": f"{wire_covered}/{wire_total}",
        "builtin_tools": f"{tools_covered}/{tools_total}",
        "rpc_methods": f"{len(matrix['rpc_methods'])}/{len(matrix['rpc_methods'])}",
        "events": f"{len(matrix['events'])}",
    }

    output = json.dumps(matrix, indent=2) + "\n"
    if args.output:
        args.output.write_text(output)
        print(f"Feature matrix written to {args.output}", file=sys.stderr)
    else:
        print(output, end="")


if __name__ == "__main__":
    main()
