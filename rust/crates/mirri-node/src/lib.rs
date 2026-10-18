//! mirri-node: NAPI binding for the Mirri Code Rust engine.
//!
//! This crate compiles to a `.node` cdylib that Node.js loads via
//! `require()`. It embeds the Rust engine directly in the Node.js
//! process — no network, no IPC, zero-cost FFI.
//!
//! ## Architecture
//!
//! ```text
//! Node.js ──require()──► .node file ──► mirri-node
//!   │                                       │
//!   │  JS calls                             │  Rust engine
//!   │  callMethod(name, params)             │  mirri-embedded (EmbeddedServer)
//!   │  subscribeEvents()                    │  mirri-core
//!   │  onReverseRpc(handler)                │
//!   │                                       │
//!   ◄─── events via ThreadsafeFunction ────┘
//!   ◄─── reverse RPC via TSFN ─────────────┘
//! ```
//!
//! ## Status
//!
//! The bridge ([`EngineBridge`]) drives an
//! [`EmbeddedServer`](mirri_embedded::EmbeddedServer) wired to `mirri-core`,
//! so every RPC method runs a real engine turn: session lifecycle,
//! `message.send` in wait/fire mode, and a streaming session event channel.
//! The NAPI exports (`napi_exports`) are synchronous stubs that prove the
//! FFI bridge (create → call → error propagation → dispose); the full async
//! surface (`callMethod` Promise, `onEvent` / `onReverseRpc` via
//! ThreadsafeFunction) lands alongside the remaining Phase 3 wiring.

pub mod engine_bridge;
pub mod napi_exports;

#[cfg(test)]
mod engine_e2e_tests;

pub use engine_bridge::EngineBridge;
