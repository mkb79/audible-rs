//! Core library of `audible-rs`, a Rust reimplementation of
//! [mkb79/Audible](https://github.com/mkb79/Audible) and
//! [mkb79/audible-cli](https://github.com/mkb79/audible-cli).
//!
//! The public API surface is `api`, `auth` and `models`; the remaining
//! modules back the `audible` binary. Planning, roadmap and decisions
//! live in Linear (the single source of truth); the archived architecture
//! spec (v4) is kept under `docs/archive/` for historical reference.

pub mod api;
pub mod auth;
pub mod models;
pub mod naming;

pub mod activation;
pub mod catalog;
pub mod commands;
pub mod config;
pub mod crypto;
pub mod db;
pub mod downloader;
pub(crate) mod fsutil;
pub mod output;
pub mod plugins;
pub mod session;
pub(crate) mod timefmt;
pub mod widevine;
