//! Standalone tool that emits the schemars-generated `WireOp` JSON Schema.
//!
//! Run with:
//!   cargo run -p mirri-proto --example schema_compare [output_path]
//!
//! The emitted file can be diffed against the TypeScript-side
//! `zod-to-json-schema` output to verify wire-format compatibility.

use mirri_proto::wire_op_schema;
use std::fs;

fn main() {
    let schema = wire_op_schema();
    let schema_json = serde_json::to_string_pretty(&schema).unwrap();

    let output_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "rust/schema_compare_output.json".to_string());

    fs::write(&output_path, &schema_json).expect("failed to write schema");
    eprintln!("Schema written to {output_path}");
    eprintln!("Compare with: npx zod-to-json-schema in the TS project");
}
