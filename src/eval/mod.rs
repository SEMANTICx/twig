//! Evaluation, failure classification, trajectory contracts, and protected self-improvement.

pub mod ablation;
pub mod classify;
pub mod dataset;
pub mod improve;
pub mod lessons;
pub mod rci;
pub mod runner;
pub mod trajectory;

pub use dataset::{import_snapshot, load_tasks, ImportTask, Split, Task};
pub use runner::{judge, run_all, run_one, summarize, Spec};
