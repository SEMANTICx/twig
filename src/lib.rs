//! twig — a thin agent harness in Rust.
//!
//! magpie provides the models; twig provides the loop, the verifier and the record.
//!
//! Architecture layers (strict DAG, deep modules, narrow interfaces):
//!   Foundation : `config`, `record`, `control`
//!   Execution  : `exec` (bounded processes, session shells, bwrap isolation)
//!   Model      : `llm` (provider, bounded SSE streaming, stub, telemetry)
//!   Verify     : `verify` (differential replay & admission gates)
//!   Runtime    : `runtime` (daemon, turn loop, tools, policy, guard, context, guidance, subagent, terminal)
//!   Evaluation : `eval` (scoring runner, protected datasets, improvement optimizer, lessons, ablation, classify, trajectory)
//!   Automation : `automation` (durable GitHub maintenance queue, isolated worktrees & shipping)

pub mod automation;
pub mod cli;
pub mod config;
pub mod control;
pub mod eval;
pub mod exec;
pub mod llm;
pub mod record;
pub mod runtime;
pub mod tui;
pub mod verify;
pub mod web;

// Narrow public façade & compatibility re-exports for CLI and integration tests.
pub use config::paths;
pub use eval::{classify, improve};
pub use exec::isolation;
pub use exec as process;
pub use llm as provider;
pub use llm::{stub, Llm, Provider};
pub use record::{Decisions, Record};
pub use runtime::{
    guard::{Guard, Level},
    guidance as playbook, policy, subagent, Daemon,
};
pub use verify::{Candidate, Verdict};
