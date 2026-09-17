//! magictree: per-worktree development stacks.
//!
//! A repository describes its stack in `magictree.toml`; magictree resolves
//! worktree identity, allocates ports, materialises environment, bootstraps the
//! checkout, and supervises services.

mod banner;
pub mod bootstrap;
pub mod cli;
pub mod compose;
pub mod config;
pub mod ctx;
pub mod discover;
pub mod doctor;
pub mod dryrun;
pub mod env;
pub mod health;
pub mod init;
pub mod manifest;
pub mod paths;
pub mod ports;
pub mod repo;
pub mod run;
pub mod slug;
pub mod update;
pub mod worktrees;

pub use cli::{dispatch, parse, Cli};
pub use config::Config;
pub use ctx::Ctx;
pub use paths::Paths;
pub use repo::Repo;
