//! `rpc` — the RPC contract fact source (plan `rust-rpc-contract-unification.md`).
//!
//! This module turns `mirri-proto` from a persistence-log crate into the
//! contract crate: it records the **facts** of the RPC surface shared by the
//! two engines —
//!
//! - [`registry`]: the static method registry — every method name either
//!   backend dispatches, with its [`BackendAvailability`] and error-semantics
//!   declaration (M1.1: names + availability only; typed params/results land
//!   per-domain in M2/M3, decision D3).
//! - [`error`]: the single JSON-RPC error-code mapping (decision D5) —
//!   the shared code constants and the domain-code → JSON-RPC-code mapping
//!   both backends converge on.
//!
//! Per review decision R5 the registry models facts only (names, types,
//! errors, availability): no behavior, no generic machinery. Handlers keep
//! living in their engines; this crate is what they agree *on*.

pub mod error;
pub mod registry;

pub use error::{
    JSON_RPC_INTERNAL, JSON_RPC_INVALID_PARAMS, JSON_RPC_METHOD_NOT_FOUND, JSON_RPC_NOT_FOUND,
    JSON_RPC_UNAUTHORIZED, RUST_ONLY_DOMAIN_CODES, RUST_PRODUCED_TS_CODES, TS_ONLY_DOMAIN_CODES,
    domain_code_retryable, domain_error_data, json_rpc_code_for_domain_code,
};
pub use registry::{
    BackendAvailability, METHODS, MethodBatch, MethodDisposition, MethodDomain, MethodEntry,
    MethodErrorSemantics, RpcBackend, TypedMethod, all_methods, current_names_for, get,
    methods_for,
};
