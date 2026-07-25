// The GLB auditor, shared verbatim with tests/glb_budgets.rs so the
// enforcement logic is unit-tested (build scripts have no #[test] path).
mod glb_audit {
    include!("build/glb_audit.rs");
}

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

/// Fails the build when any file in `assets/objects/` (all of which the
/// `EmbeddedObjects` registry embeds into every binary and the wasm bundle,
/// and every one of which is wire-loadable via `object.add`) breaches its
/// byte budget, or when any `.glb` fails the static audit (#23 §3:
/// build-verified triangle/texture/material/decoded-runtime caps).
fn enforce_object_budgets() -> std::io::Result<()> {
    let dir = std::path::Path::new("assets/objects");
    let mut total: u64 = 0;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        let len = entry.metadata()?.len();
        if len > glb_audit::OBJECT_ASSET_BUDGET_BYTES {
            return Err(std::io::Error::other(format!(
                "object asset budget breached: {} is {len} bytes, over the \
                 {}-byte per-asset budget",
                path.display(),
                glb_audit::OBJECT_ASSET_BUDGET_BYTES
            )));
        }
        total += len;
        let extension = path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
        if extension == "gltf" {
            return Err(std::io::Error::other(format!(
                "GLB audit failed: {}: embedded scene assets must be \
                 self-contained .glb (a .gltf references external files)",
                path.display()
            )));
        }
        if extension == "glb" {
            let bytes = std::fs::read(&path)?;
            let audit = glb_audit::audit_glb(&bytes)
                .and_then(|audit| glb_audit::enforce_glb_caps(&audit).map(|()| audit));
            if let Err(message) = audit {
                return Err(std::io::Error::other(format!(
                    "GLB audit failed: {}: {message}",
                    path.display()
                )));
            }
        }
    }
    if total > glb_audit::OBJECT_PACKAGE_BUDGET_BYTES {
        return Err(std::io::Error::other(format!(
            "object package budget breached: assets/objects/ totals {total} bytes, \
             over the {}-byte package budget",
            glb_audit::OBJECT_PACKAGE_BUDGET_BYTES
        )));
    }
    Ok(())
}

fn main() -> std::io::Result<()> {
    println!("cargo:rerun-if-changed=assets/ratty.ico");
    println!("cargo:rerun-if-changed=assets/sounds");
    println!("cargo:rerun-if-changed=assets/objects");
    println!("cargo:rerun-if-changed=build.rs");
    // The auditor's own code must re-trigger the build, or edits to it
    // (including ones that weaken it) would go unenforced until an
    // unrelated trigger fired — "build-verified" has to survive drift.
    println!("cargo:rerun-if-changed=build/glb_audit.rs");

    enforce_sound_budgets()?;
    enforce_object_budgets()?;

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
