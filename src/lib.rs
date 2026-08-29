#![forbid(unsafe_code)]
#![allow(
    clippy::duration_suboptimal_units,
    reason = "runtime and provider timeout policy is intentionally expressed in one seconds-based unit"
)]

pub mod acp;
pub mod auth;
pub mod budget;
pub mod cli;
pub mod config;
pub mod daemon;
pub mod error;
pub mod extensions;
pub mod mcp;
pub mod migration;
pub mod model;
pub mod observation;
pub mod orchestration;
pub mod provider;
pub mod refinement;
pub mod resources;
pub mod rpc;
pub mod runtime;
pub mod runtime_events;
pub mod session;
pub mod session_compat;
pub mod session_integrity;
pub mod session_tree;
pub mod skills;
pub mod tools;
pub mod tui;

mod atomic;
