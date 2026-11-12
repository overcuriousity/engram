#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
//! engram: a self-hosted personal knowledge base.
//! A library too, so the integration suite drives the code the binary runs.

pub mod auth;
pub mod backup;
pub mod cli;
pub mod config;
#[cfg(feature = "contained")]
pub mod contained;
pub mod core;
pub mod error;
pub mod eval;
pub mod fmt;
pub mod infer;
pub mod jobs;
pub mod mcp;
pub mod store;
pub mod tenants;
pub mod vector;
pub mod web;
