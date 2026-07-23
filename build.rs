/// Per-asset byte budget for embedded sounds under `assets/sounds/`.
const SOUND_ASSET_BUDGET_BYTES: u64 = 192 * 1024;

/// Whole-package byte budget for `assets/sounds/` as a set.
const SOUND_PACKAGE_BUDGET_BYTES: u64 = 512 * 1024;

/// Fails the build when any file in `assets/sounds/` (all of which the
/// `EmbeddedSounds` registry embeds into every binary and the wasm bundle)
/// breaches its per-asset budget, or the set breaches the package budget.
fn enforce_sound_budgets() -> std::io::Result<()> {
    let dir = std::path::Path::new("assets/sounds");
    let mut total: u64 = 0;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let len = entry.metadata()?.len();
        if len > SOUND_ASSET_BUDGET_BYTES {
            return Err(std::io::Error::other(format!(
                "sound asset budget breached: {} is {len} bytes, over the \
                 {SOUND_ASSET_BUDGET_BYTES}-byte per-asset budget",
                entry.path().display()
            )));
        }
        total += len;
    }
    if total > SOUND_PACKAGE_BUDGET_BYTES {
        return Err(std::io::Error::other(format!(
            "sound package budget breached: assets/sounds/ totals {total} bytes, \
             over the {SOUND_PACKAGE_BUDGET_BYTES}-byte package budget"
        )));
    }
    Ok(())
}

fn main() -> std::io::Result<()> {
    println!("cargo:rerun-if-changed=assets/ratty.ico");
    println!("cargo:rerun-if-changed=assets/sounds");
    println!("cargo:rerun-if-changed=build.rs");

    enforce_sound_budgets()?;

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return Ok(());
    }

    let mut resource = winresource::WindowsResource::new();
    resource.set_icon("assets/ratty.ico").set_manifest(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <application>
    <windowsSettings>
      <consoleAllocationPolicy xmlns="http://schemas.microsoft.com/SMI/2024/WindowsSettings">detached</consoleAllocationPolicy>
    </windowsSettings>
  </application>
</assembly>
"#,
    );

    resource.compile()
}
