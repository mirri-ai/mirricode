//! Build script — required by the napi-rs v2 build pipeline.
//!
//! `napi_build::setup()` emits the N-API version cfg flags (e.g. `napi8`)
//! that the `napi` crate and the `#[napi]` macro-generated code depend on,
//! and records the build environment for `napi build` (the `@napi-rs/cli`
//! tool) to consume. Without this script the cdylib's module registration
//! may not match the requested N-API version.

fn main() {
    napi_build::setup();
}
