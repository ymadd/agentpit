//! Blueprint loops: the runner side of docs/workspace-loop-design.md.
//!
//! The schema (blueprints, journal records, the fold and its transition table) lives in
//! `agentpit_events::loops` so the dashboard folds journals with the same code. This module
//! holds what only the runner needs: the scheduler that decides the next record, and the
//! translation of its decisions into records.

pub mod classify;
pub mod control;
pub mod effects;
pub mod paths;
pub mod proc;
pub mod prompt;
pub mod records;
pub mod runner;
pub mod sched;

#[cfg(test)]
mod runner_tests;
#[cfg(test)]
mod sim;
