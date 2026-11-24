//! twig — a thin agent harness.
//!
//! magpie provides the models; this provides the loop, the verifier and the record.
//!
//! Design constraints, in priority order:
//!   1. defence never sits in the exec path
//!   2. one guard, not two
//!   3. nothing is enabled until it is proven on recorded real calls
//!   4. the record is append-only and hash-chained

pub mod ablation;
pub mod classify;
pub mod config;
pub mod daemon;
pub mod decisions;
pub mod eval;
pub mod guard;
pub mod improve;
pub mod outer;
pub mod playbook;
pub mod policy;
pub mod provider;
pub mod process;
pub mod record;
pub mod stub;
pub mod telemetry;
pub mod tools;
pub mod trajectory;
pub mod turn;
pub mod verify;

pub use daemon::Daemon;
pub use guard::{Guard, Level};
pub use provider::{Llm, Provider};
pub use record::Record;
pub use verify::{Candidate, Verdict};
