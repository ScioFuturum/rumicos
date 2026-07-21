//! The Rumicos shell, as a library so its pure half is host-testable.
//!
//! * [`parse`] — pure: tokenizer + pipeline planner. No unsafe, no
//!   syscalls, no allocation. This is where all the logic lives and where
//!   all the unit tests point.
//! * [`exec`] — syscall glue: fork/dup2/execve/wait4 choreography. Consumes
//!   only an already-validated [`parse::Pipeline`].
//! * [`input`] — reading a line off the raw serial line.
//!
//! `cargo test -p shell` builds this for the host (std is linked only for
//! the test harness); the actual binary is `src/main.rs`, built with
//! `--target x86_64-unknown-none --features bin`.

#![cfg_attr(not(test), no_std)]
#![deny(unsafe_op_in_unsafe_fn)]

pub mod exec;
pub mod input;
pub mod parse;
pub mod selftest;
