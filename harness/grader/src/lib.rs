//! Library surface for the grader crate.
//!
//! Exposes the wasmtime + fuel + import-stub wiring used by the grader
//! binary so other tools (e.g. the oracle) can reuse `WasmCandidate`
//! without duplicating it.

pub mod corpus;
pub mod passfail;
pub mod ref_cache;
pub mod wasm_candidate;
