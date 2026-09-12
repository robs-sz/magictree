//! magictree: per-worktree development stacks.
//!
//! A repository describes its stack in `magictree.toml`; magictree resolves
//! worktree identity, allocates ports, materialises environment, bootstraps the
//! checkout, and supervises services.

pub mod paths;
pub mod repo;
pub mod slug;
