//! engram: a self-hosted personal knowledge base.
//!
//! Exposed as a library so the integration suite can drive the same code the
//! binary runs.

pub mod auth;
pub mod config;
pub mod core;
pub mod error;
pub mod infer;
pub mod jobs;
pub mod mcp;
pub mod store;
pub mod vector;
pub mod web;
