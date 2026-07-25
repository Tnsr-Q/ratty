//! Unit tests for the build-time GLB auditor (#23 §3).
//!
//! `build/glb_audit.rs` is included verbatim (the same way `build.rs`
//! includes it), so the enforcement logic that gates the build is itself
//! under test: the shipped assets pass with their measured numbers, and
//! synthetic corpora fail with the exact invariant messages.

#[allow(dead_code)]
mod glb_audit {
    include!("../build/glb_audit.rs");
}

use glb_audit::{GLB_MAX_TRIANGLES, audit_glb, enforce_glb_caps};

fn read_asset(name: &str) -> Vec<u8> {
    std::fs::read(format!("assets/objects/{name}")).expect("shipped asset reads")
}

/// Builds a minimal syntactically-valid GLB around the given JSON chunk
/// (and optional BIN chunk).
fn synthetic_glb(json: &str, bin: Option<&[u8]>) -> Vec<u8> {
    let mut json_bytes = json.as_bytes().to_vec();
    while !json_bytes.len().is_multiple_of(4) {
        json_bytes.push(b' ');
    }
    let mut out = Vec::new();
    out.extend_from_slice(b"glTF");
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // patched below
    out.extend_from_slice(&(json_bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(&0x4E4F_534Au32.to_le_bytes());
    out.extend_from_slice(&json_bytes);
    if let Some(bin) = bin {
        let mut bin_bytes = bin.to_vec();
        while !bin_bytes.len().is_multiple_of(4) {
            bin_bytes.push(0);
        }
        out.extend_from_slice(&(bin_bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(&0x004E_4942u32.to_le_bytes());
        out.extend_from_slice(&bin_bytes);
    }
    let len = out.len() as u32;
    out[8..12].copy_from_slice(&len.to_le_bytes());
    out
}

/// A PNG header (signature + IHDR) with the given dimensions — enough for
/// the auditor, which never decodes past the IHDR.
fn png_header(width: u32, height: u32) -> Vec<u8> {
    let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    png.extend_from_slice(&13u32.to_be_bytes());
    png.extend_from_slice(b"IHDR");
    png.extend_from_slice(&width.to_be_bytes());
    png.extend_from_slice(&height.to_be_bytes());
    png.extend_from_slice(&[8, 6, 0, 0, 0]);
    png.extend_from_slice(&[0, 0, 0, 0]); // CRC, unchecked by the auditor
    png
}

#[test]
fn shipped_glbs_pass_with_measured_numbers() {
    let audit = audit_glb(&read_asset("SpinyMouse.glb")).expect("mascot audits");
    assert_eq!(audit.triangles, 818);
    assert_eq!(audit.materials, 1);
    assert_eq!(audit.images, vec![(2048, 2048)]);
    assert_eq!(
        audit.decoded_estimate_bytes,
        audit.bin_bytes + 2048 * 2048 * 4
    );
    enforce_glb_caps(&audit).expect("mascot fits every cap");

    let audit = audit_glb(&read_asset("Ferris.glb")).expect("Ferris audits");
    assert_eq!(audit.triangles, 274);
    assert_eq!(audit.materials, 4);
    assert!(audit.images.is_empty());
    enforce_glb_caps(&audit).expect("Ferris fits every cap");
}

#[test]
fn container_framing_failures_are_named() {
    let err = audit_glb(b"noGL").expect_err("short file fails");
    assert!(err.contains("12-byte GLB header"), "{err}");

    let mut glb = synthetic_glb("{}", None);
    glb[0..4].copy_from_slice(b"nope");
    assert!(audit_glb(&glb).expect_err("bad magic").contains("magic"));

    let mut glb = synthetic_glb("{}", None);
    glb[4..8].copy_from_slice(&1u32.to_le_bytes());
    assert!(
        audit_glb(&glb)
            .expect_err("version 1")
            .contains("version is 1")
    );

    let mut glb = synthetic_glb("{}", None);
    glb[8..12].copy_from_slice(&9999u32.to_le_bytes());
    let err = audit_glb(&glb).expect_err("bad declared length");
    assert!(err.contains("declared length"), "{err}");
}

#[test]
fn compression_extensions_are_rejected() {
    let glb = synthetic_glb(
        r#"{"extensionsRequired":["KHR_draco_mesh_compression"]}"#,
        None,
    );
    let err = audit_glb(&glb).expect_err("draco rejects");
    assert!(err.contains("KHR_draco_mesh_compression"), "{err}");
    assert!(err.contains("re-export"), "{err}");
}

#[test]
fn external_uris_are_rejected() {
    let glb = synthetic_glb(r#"{"buffers":[{"uri":"model.bin","byteLength":4}]}"#, None);
    let err = audit_glb(&glb).expect_err("external buffer rejects");
    assert!(err.contains("self-contained"), "{err}");

    let glb = synthetic_glb(r#"{"images":[{"uri":"skin.png"}]}"#, None);
    let err = audit_glb(&glb).expect_err("external image rejects");
    assert!(err.contains("self-contained"), "{err}");
}

#[test]
fn non_png_textures_are_rejected() {
    let glb = synthetic_glb(
        r#"{"images":[{"mimeType":"image/jpeg","bufferView":0}],
            "bufferViews":[{"buffer":0,"byteLength":4}]}"#,
        Some(&[0, 0, 0, 0]),
    );
    let err = audit_glb(&glb).expect_err("jpeg rejects");
    assert!(err.contains("image/png"), "{err}");
}

#[test]
fn oversized_textures_and_triangles_fail_the_caps() {
    let png = png_header(4096, 4096);
    let json = format!(
        r#"{{"images":[{{"mimeType":"image/png","bufferView":0}}],
             "bufferViews":[{{"buffer":0,"byteOffset":0,"byteLength":{}}}]}}"#,
        png.len()
    );
    let audit = audit_glb(&synthetic_glb(&json, Some(&png))).expect("audits");
    assert_eq!(audit.images, vec![(4096, 4096)]);
    let err = enforce_glb_caps(&audit).expect_err("4096 texture over cap");
    assert!(err.contains("4096x4096"), "{err}");

    let over = (GLB_MAX_TRIANGLES + 1) * 3;
    let json = format!(
        r#"{{"meshes":[{{"primitives":[{{"indices":0,"attributes":{{"POSITION":1}}}}]}}],
             "accessors":[{{"count":{over}}},{{"count":3}}]}}"#
    );
    let audit = audit_glb(&synthetic_glb(&json, None)).expect("audits");
    let err = enforce_glb_caps(&audit).expect_err("triangles over cap");
    assert!(err.contains("triangle budget"), "{err}");
}

#[test]
fn non_triangle_primitives_are_rejected() {
    let glb = synthetic_glb(
        r#"{"meshes":[{"primitives":[{"mode":1,"attributes":{"POSITION":0}}]}],
            "accessors":[{"count":6}]}"#,
        None,
    );
    let err = audit_glb(&glb).expect_err("lines reject");
    assert!(err.contains("mode 1"), "{err}");
}
