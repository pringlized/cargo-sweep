//! Clean stale Rust build caches across a Linux dev machine.
//!
//! The rules, in the order they protect you:
//! - Anything named `release` is never touched, at any depth. No flag overrides it.
//! - A target dir that a running process executes from, or names on its command line, is skipped
//!   whole (see [`live`]).
//! - Only build output nobody has used for N days is removed (see [`plan`]), so a project you are
//!   working on keeps its incremental cache.
//! - Nothing is deleted unless the caller asks for it (`--apply`).

pub mod discover;
pub mod live;
pub mod plan;
pub mod sweep;

#[cfg(test)]
pub(crate) mod testutil;
