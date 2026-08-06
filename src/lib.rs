//! Ratty terminal runtime and rendering library.
//!
//! This crate provides the terminal runtime, scene integration, protocol handling and widget
//! plumbing for Ratty.

#![warn(missing_docs)]
#![warn(clippy::unwrap_used)]
// The `caps` reply's append-only limits map (query_channel.rs) grew past
// serde_json's per-entry macro recursion at the default 128 when the
// presence limits landed (#25).
#![recursion_limit = "256"]

pub mod ai;
pub mod avatar;
pub mod bookmarks;
pub mod capability;
pub mod cli;
pub mod config;
mod direct_render;
pub mod effects;
pub mod focus;
mod fonts;
pub mod identity;
pub mod inline;
pub mod keyboard;
pub mod kitty;
pub mod macros;
pub mod model;
pub mod mouse;
pub mod osc;
pub mod paths;
pub mod plugin;
pub mod presence;
pub mod present;
pub mod query;
pub mod query_channel;
pub mod reactive;
pub mod rendering;
pub mod rgp;
pub mod runtime;
pub mod scene;
pub mod sound;
pub mod systems;
pub mod terminal;
pub mod viz;
mod viz_draw;
pub mod viz_wire;
pub mod web;
