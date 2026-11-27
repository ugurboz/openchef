//! Leak-free child process supervision: process groups, Job Objects, start tokens.
//!
//! This crate talks to the operating system directly, so `unsafe` is allowed here
//! (D-030). Every `unsafe` block must carry a `// SAFETY:` comment.
#![allow(unsafe_code)]
