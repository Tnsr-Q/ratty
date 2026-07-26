// Static GLB audit for the embedded object budget (#23 §3).
//
// Everything checked here is honestly decidable from the GLB container
// alone — no GPU, no image decoder: container framing, triangle counts
// from accessor metadata, material/image counts, PNG dimensions from the
// IHDR header, self-containment (no external `uri`), and a decoded-runtime
// *estimate* (raw BIN bytes + Σ width×height×4 RGBA8 texture bytes).
// Compression extensions are rejected outright rather than assumed: this
// build's bevy_gltf decodes no `extensionsRequired` extension (no Draco,
// no meshopt, no basisu), so such a file would fail at runtime anyway —
// the audit moves that failure to compile time.
//
// NOT verifiable here, deliberately unclaimed: device GPU memory truth
// (compressed-format support is resolved at `GltfPlugin::finish`), mipmap
// allocation, animation runtime cost, and which scene index is intended.
//
// Shared verbatim between `build.rs` (via `include!`) and
// `tests/glb_budgets.rs`, so the enforcement logic itself is unit-tested.

/// Per-asset byte budget for embedded objects under `assets/objects/`
/// (the locked #23 §3 cap: ≤ 2 MiB packaged). SpinyMouse.glb, the avatar
/// mascot, is 1,684,108 bytes — 80% of this.
pub const OBJECT_ASSET_BUDGET_BYTES: u64 = 2 * 1024 * 1024;
/// Whole-package byte budget for `assets/objects/` as a set (all of it is
/// embedded into every binary and the wasm bundle via `EmbeddedObjects`).
pub const OBJECT_PACKAGE_BUDGET_BYTES: u64 = 4 * 1024 * 1024;
/// Triangle cap per GLB — "low-poly" made falsifiable. Shipped: 818 / 274.
pub const GLB_MAX_TRIANGLES: u64 = 5_000;
/// Material cap per GLB. Shipped: 1 / 4.
pub const GLB_MAX_MATERIALS: u64 = 8;
/// Embedded image cap per GLB; every image must be PNG (the only compiled
/// bevy image format) and at most [`GLB_MAX_TEXTURE_DIM`] per side.
pub const GLB_MAX_IMAGES: u64 = 4;
/// Maximum texture dimension per side, read from the PNG IHDR.
pub const GLB_MAX_TEXTURE_DIM: u32 = 2048;
/// Decoded-runtime estimate cap: BIN chunk bytes + Σ(W×H×4) RGBA8 texture
/// bytes. An estimate of CPU-side decode + GPU upload, NOT device truth
/// (see the module doc). SpinyMouse.glb: 18,459,172 bytes — 88% of this.
pub const GLB_MAX_DECODED_BYTES: u64 = 20 * 1024 * 1024;

/// Statically audited facts about one GLB container.
// The build script reads only the enforced fields; tests read them all.
#[allow(dead_code)]
#[derive(Debug)]
pub struct GlbAudit {
    /// Triangle count summed over every mesh primitive.
    pub triangles: u64,
    /// `materials.len()`.
    pub materials: u64,
    /// `(width, height)` of each embedded image, PNG-only enforced.
    pub images: Vec<(u32, u32)>,
    /// The BIN chunk's byte length (0 when absent).
    pub bin_bytes: u64,
    /// BIN bytes + Σ(W×H×4) decoded RGBA8 texture bytes.
    pub decoded_estimate_bytes: u64,
}

fn read_u32_le(bytes: &[u8], offset: usize) -> Result<u32, String> {
    bytes
        .get(offset..offset + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or_else(|| format!("truncated at byte {offset}"))
}

fn json_u64(value: &serde_json::Value) -> Option<u64> {
    value.as_u64()
}

/// Parses and audits a GLB container. Errors are complete human sentences
/// naming the violated invariant; the caller prefixes the file path.
pub fn audit_glb(bytes: &[u8]) -> Result<GlbAudit, String> {
    // ── Container framing ──
    if bytes.len() < 12 {
        return Err("file is shorter than the 12-byte GLB header".to_string());
    }
    if &bytes[0..4] != b"glTF" {
        return Err("magic is not 'glTF'".to_string());
    }
    let version = read_u32_le(bytes, 4)?;
    if version != 2 {
        return Err(format!("container version is {version}, not 2"));
    }
    let declared = read_u32_le(bytes, 8)? as usize;
    if declared != bytes.len() {
        return Err(format!(
            "declared length {declared} does not match file length {}",
            bytes.len()
        ));
    }

    // ── Chunk walk: JSON first, optional BIN ──
    let mut offset = 12usize;
    let mut json_chunk: Option<&[u8]> = None;
    let mut bin_chunk: Option<&[u8]> = None;
    while offset < bytes.len() {
        let length = read_u32_le(bytes, offset)? as usize;
        let kind = read_u32_le(bytes, offset + 4)?;
        let start = offset + 8;
        let end = start
            .checked_add(length)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| format!("chunk at byte {offset} overruns the file"))?;
        match kind {
            0x4E4F_534A => {
                if json_chunk.is_some() {
                    return Err("more than one JSON chunk".to_string());
                }
                if bin_chunk.is_some() {
                    return Err("JSON chunk appears after the BIN chunk".to_string());
                }
                json_chunk = Some(&bytes[start..end]);
            }
            0x004E_4942 => {
                if bin_chunk.is_some() {
                    return Err("more than one BIN chunk".to_string());
                }
                bin_chunk = Some(&bytes[start..end]);
            }
            other => return Err(format!("unknown chunk type 0x{other:08x}")),
        }
        // Chunks are 4-byte aligned.
        offset = end + (4 - end % 4) % 4;
    }
    let json_chunk = json_chunk.ok_or_else(|| "no JSON chunk".to_string())?;
    let bin = bin_chunk.unwrap_or(&[]);

    let root: serde_json::Value = serde_json::from_slice(json_chunk)
        .map_err(|error| format!("JSON chunk does not parse: {error}"))?;

    // ── Compression / extension posture ──
    if let Some(required) = root.get("extensionsRequired").and_then(|v| v.as_array())
        && !required.is_empty()
    {
        let list = required
            .iter()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "extensionsRequired lists [{list}]; this build's bevy_gltf decodes no \
             required extensions (Draco/meshopt/basisu are not compiled in) — \
             re-export without compression extensions"
        ));
    }

    // ── Self-containment ──
    let empty = Vec::new();
    let buffers = root
        .get("buffers")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty);
    for (index, buffer) in buffers.iter().enumerate() {
        if let Some(uri) = buffer.get("uri").and_then(|v| v.as_str()) {
            return Err(format!(
                "buffers[{index}] references external uri '{uri}'; embedded scene \
                 assets must be self-contained"
            ));
        }
    }

    // ── Triangles ──
    let accessors = root
        .get("accessors")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty);
    let meshes = root
        .get("meshes")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty);
    let mut triangles: u64 = 0;
    for (mesh_index, mesh) in meshes.iter().enumerate() {
        let primitives = mesh
            .get("primitives")
            .and_then(|v| v.as_array())
            .unwrap_or(&empty);
        for (prim_index, primitive) in primitives.iter().enumerate() {
            let mode = primitive.get("mode").and_then(json_u64).unwrap_or(4);
            if mode != 4 {
                return Err(format!(
                    "meshes[{mesh_index}].primitives[{prim_index}] uses mode {mode}; \
                     only TRIANGLES (4) is budgeted"
                ));
            }
            let count_accessor = primitive
                .get("indices")
                .and_then(json_u64)
                .or_else(|| {
                    primitive
                        .get("attributes")
                        .and_then(|attrs| attrs.get("POSITION"))
                        .and_then(json_u64)
                })
                .ok_or_else(|| {
                    format!(
                        "meshes[{mesh_index}].primitives[{prim_index}] has neither \
                         indices nor a POSITION attribute"
                    )
                })?;
            let count = accessors
                .get(count_accessor as usize)
                .and_then(|accessor| accessor.get("count"))
                .and_then(json_u64)
                .ok_or_else(|| {
                    format!("accessor {count_accessor} is missing or has no count")
                })?;
            triangles += count / 3;
        }
    }

    // ── Materials and images ──
    let materials = root
        .get("materials")
        .and_then(|v| v.as_array())
        .map_or(0, |m| m.len() as u64);
    let buffer_views = root
        .get("bufferViews")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty);
    let image_entries = root
        .get("images")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty);
    let mut images = Vec::new();
    for (index, image) in image_entries.iter().enumerate() {
        if let Some(uri) = image.get("uri").and_then(|v| v.as_str()) {
            return Err(format!(
                "images[{index}] references external uri '{uri}'; embedded scene \
                 assets must be self-contained"
            ));
        }
        let mime = image.get("mimeType").and_then(|v| v.as_str()).unwrap_or("");
        if mime != "image/png" {
            return Err(format!(
                "images[{index}] mimeType is '{mime}'; only image/png decodes \
                 (Cargo.toml compiles only the png image format)"
            ));
        }
        let view_index = image
            .get("bufferView")
            .and_then(json_u64)
            .ok_or_else(|| format!("images[{index}] has no bufferView"))?;
        let view = buffer_views
            .get(view_index as usize)
            .ok_or_else(|| format!("images[{index}] bufferView {view_index} is missing"))?;
        let view_offset = view.get("byteOffset").and_then(json_u64).unwrap_or(0) as usize;
        let view_length = view
            .get("byteLength")
            .and_then(json_u64)
            .ok_or_else(|| format!("bufferViews[{view_index}] has no byteLength"))?
            as usize;
        let png = bin
            .get(view_offset..view_offset + view_length)
            .ok_or_else(|| format!("images[{index}] bufferView overruns the BIN chunk"))?;
        // PNG mandates the IHDR chunk first: width/height are big-endian
        // u32 at bytes 16/20 — no decoder needed.
        if png.len() < 24 || png[0..8] != [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A] {
            return Err(format!("images[{index}] bytes are not a PNG"));
        }
        let width = u32::from_be_bytes([png[16], png[17], png[18], png[19]]);
        let height = u32::from_be_bytes([png[20], png[21], png[22], png[23]]);
        images.push((width, height));
    }

    let bin_bytes = bin.len() as u64;
    let decoded_estimate_bytes = bin_bytes
        + images
            .iter()
            .map(|(w, h)| u64::from(*w) * u64::from(*h) * 4)
            .sum::<u64>();

    Ok(GlbAudit {
        triangles,
        materials,
        images,
        bin_bytes,
        decoded_estimate_bytes,
    })
}

/// Checks an audited GLB against the budget constants. Errors name the
/// measured value, the budget, and the remedy; the caller prefixes the
/// file path.
pub fn enforce_glb_caps(audit: &GlbAudit) -> Result<(), String> {
    if audit.triangles > GLB_MAX_TRIANGLES {
        return Err(format!(
            "{} triangles, over the {GLB_MAX_TRIANGLES}-triangle budget",
            audit.triangles
        ));
    }
    if audit.materials > GLB_MAX_MATERIALS {
        return Err(format!(
            "{} materials, over the {GLB_MAX_MATERIALS}-material budget",
            audit.materials
        ));
    }
    if audit.images.len() as u64 > GLB_MAX_IMAGES {
        return Err(format!(
            "{} embedded images, over the {GLB_MAX_IMAGES}-image budget",
            audit.images.len()
        ));
    }
    for (index, (width, height)) in audit.images.iter().enumerate() {
        if *width > GLB_MAX_TEXTURE_DIM || *height > GLB_MAX_TEXTURE_DIM {
            return Err(format!(
                "images[{index}] is {width}x{height}, over the \
                 {GLB_MAX_TEXTURE_DIM} per-side budget"
            ));
        }
    }
    if audit.decoded_estimate_bytes > GLB_MAX_DECODED_BYTES {
        return Err(format!(
            "decoded-runtime estimate is {} bytes (BIN + RGBA8 textures), over \
             the {GLB_MAX_DECODED_BYTES}-byte budget",
            audit.decoded_estimate_bytes
        ));
    }
    Ok(())
}
