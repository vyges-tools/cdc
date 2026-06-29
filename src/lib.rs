//! vyges-cdc — structural **clock-domain-crossing** check.
//!
//! A gate-level **netlist** + a **Liberty** (to know which cells are flops and
//! their clock/data pins) + an **SDC** (the clock definitions) in, the list of
//! **domain crossings** out — each flagged synchronized or not. This is a purely
//! *structural*, deterministic graph analysis: it asks "which signals are launched
//! in one clock domain and captured in another, and do they pass through a
//! synchronizer?" — a question a lockstep simulator structurally cannot answer.
//!
//! v0 assigns each flop a domain by tracing its clock pin back (through clock
//! buffers/inverters) to an SDC clock source, walks each flop's data cone back to
//! its launching flops, reports every cross-domain launch→capture pair, and
//! recognizes the canonical **two-flop synchronizer** (direct Q→D crossing into a
//! flop whose Q feeds a second same-domain flop). Reconvergence, gray-code/handshake
//! recognition, and glitch/data-stability are the depth passes.
//!
//! Reads the same Liberty / Verilog / SDC the rest of Loom does. Pure std beyond
//! the shared parsers.

pub use vyges_loom::{liberty, netlist, sdc};

pub mod cdc;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const COPYRIGHT: &str = "© 2026 Vyges. All Rights Reserved.  https://vyges.com";
