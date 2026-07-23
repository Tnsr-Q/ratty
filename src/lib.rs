//! Ratty terminal runtime and rendering library.
//!
//! This crate provides the terminal runtime, scene integration, protocol handling and widget
//! plumbing for Ratty.

#![warn(missing_docs)]
#![warn(clippy::unwrap_used)]

pub mod ai;
pub mod cli;
pub mod config;
mod direct_render;
pub mod effects;
pub mod inline;
pub mod keyboard;
pub mod kitty;
pub mod model;
pub mod mouse;
pub mod osc;
pub mod paths;
pub mod plugin;
pub mod present;
pub mod query;
pub mod query_channel;
pub mod rendering;
pub mod rgp;
pub mod runtime;
pub mod scene;
pub mod systems;
pub mod terminal;
pub mod viz;
pub mod web;
