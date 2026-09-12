//! magictree: per-worktree development stacks.
//!
//! A repository describes its stack in `magictree.toml`; magictree resolves
//! worktree identity, allocates ports, materialises environment, bootstraps the
//! checkout, and supervises services.

pub mod bootstrap;
pub mod compose;
pub mod config;
pub mod ctx;
pub mod dryrun;
pub mod env;
pub mod health;
pub mod manifest;
pub mod paths;
pub mod ports;
pub mod repo;
pub mod run;
pub mod slug;
pub mod worktrees;
