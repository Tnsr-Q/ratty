//! Shared filesystem path helpers (native targets only — the web build has
//! no filesystem; assets are embedded or delivered in-band).

#![cfg(not(target_arch = "wasm32"))]

use std::path::{Path, PathBuf};

use etcetera::{BaseStrategy, choose_base_strategy};

/// Expands a leading `~` in a path using the current user's home directory.
pub fn expand_path(path: &Path) -> PathBuf {
    PathBuf::from(shellexpand::tilde(&path.to_string_lossy()).into_owned())
}

/// Returns the writable runtime asset root used for scene-backed object files.
pub fn runtime_asset_root() -> PathBuf {
    choose_base_strategy()
        .map(|strategy| strategy.cache_dir())
        .unwrap_or_else(|_| std::env::temp_dir())
        .join(env!("CARGO_PKG_NAME"))
        .join("assets")
}
