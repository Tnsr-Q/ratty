//! The stage-2 property, across both halves of the gated tee: **the only
//! presence a spectator can ever learn is a fresh row, under a mirrored id.**
//!
//! Half one is the gate: every presence byte the primary writes is excised
//! before the tee, so nothing about the real roster — ids, leases, or the
//! commands themselves — reaches a spectator through the transport.
//!
//! Half two is the mirror: what a spectator does see is synthesized from a
//! 778 walk that carries `fresh` explicitly, under ids rewritten `r<ns>.<id>`
//! inside the terminal's 48-byte cap. An expired row is never synthesized,
//! so no spectator ever learns of a row that was not lawfully public when it
//! was sent (`protocols/presence.md:121`, `protocols/query.md:238`).
//!
//! Name, colour, cursor cell, and note text *do* cross — they are what
//! presence draws, and rendered = public by definition
//! (`protocols/presence.md:37-39`). The identity key is the part that is
//! rewritten, because a caller-local id under the spectator's own namespace
//! 0 is not the same participant.

use std::time::Instant;

#[allow(dead_code)] // Shared wire module; these tests use the parser half.
#[path = "../../../src/osc.rs"]
mod osc;

#[allow(dead_code)] // The gate's injection-boundary half belongs to the host.
#[path = "../src/gate.rs"]
mod gate;

#[allow(dead_code)] // The mirror's snapshot half belongs to the fan-out.
#[path = "../src/presence.rs"]
mod presence;

use gate::{Gate, Seg};
use presence::{Cmd, MAX_MIRROR_ID_BYTES, Mirror, Row};
use serde_json::json;

/// Everything the primary's own presence traffic carries — the strings that
/// must not reach a spectator through the *transport*, whatever the mirror
/// later chooses to synthesize.
const SOURCE_ID: &str = "ann-private-id";
const SOURCE_NOTE_ID: &str = "n-secret";

/// One session's worth of real primary output: rendered content, the whole
/// presence family, and a malformed presence command whose text would leak
/// off the wire if the gate ever forwarded what it cannot classify.
fn primary_stream() -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"$ cargo test\r\n   Compiling ratty\r\n");
    out.extend_from_slice(b"\x1b]777;ratty:flash;color=green\x07");
    for command in [
        format!("user.join;id={SOURCE_ID}&name=Ann%20W&color=%2300ffcc&ttl=60"),
        format!("user.cursor;id={SOURCE_ID}&x=3&y=4"),
        format!("user.renew;id={SOURCE_ID}&ttl=60"),
        format!("note;id={SOURCE_NOTE_ID}&text=review%20this&x=15&y=10&ttl=300"),
        format!("note.remove;id={SOURCE_NOTE_ID}"),
        format!("user.leave;id={SOURCE_ID}"),
    ] {
        out.extend_from_slice(format!("\x1b]777;ratty:{command}\x07").as_bytes());
    }
    // Unparseable, so unclassifiable — and it still carries its text.
    out.extend_from_slice(b"\x1b]777;ratty:note;id=bad&text=unclassifiable\x07");
    // The relay's own query plane, both terminators.
    out.extend_from_slice(b"\x1b]778;v=1;t=q;id=tok1;op=state.presence\x1b\\");
    out.extend_from_slice(b"\x1b]778;v=1;t=r;id=tok1;ok=1;data=e30\x1b\\");
    out.extend_from_slice(b"    Finished in 0.4s\r\n");
    out
}

fn gated(stream: &[u8]) -> String {
    let mut gate = Gate::new();
    let bytes: Vec<u8> = gate
        .push(stream)
        .iter()
        .flat_map(|seg| match seg {
            Seg::Data(bytes) | Seg::Anchor { bytes, .. } => bytes.clone(),
        })
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The documented `state.presence` projection, as a walk would deliver it:
/// two fresh rows and one that has already expired. The relay's own
/// namespace comes back including expired rows, so the `fresh` flag — not
/// the caller's arithmetic — is what filters them.
fn walk() -> Vec<Row> {
    let items = json!([
        { "kind": "participant", "ns": 0, "id": SOURCE_ID, "name": "Ann W",
          "color": "#00ffcc", "cursor": { "x": 3, "y": 4 },
          "fresh": true, "age_secs": 5.0, "ttl_secs": 60.0, "revision": 2 },
        { "kind": "participant", "ns": 0, "id": "already-expired", "name": "Bo",
          "color": "#ff0000", "cursor": null,
          "fresh": false, "age_secs": 90.0, "ttl_secs": 60.0, "revision": 1 },
        { "kind": "note", "ns": 0, "id": SOURCE_NOTE_ID, "text": "review this",
          "x": 15, "y": 10,
          "fresh": true, "age_secs": 1.0, "ttl_secs": 300.0, "revision": 1 }
    ]);
    items
        .as_array()
        .expect("an items array")
        .iter()
        .filter_map(Row::from_json)
        .collect()
}

#[test]
fn the_transport_carries_no_presence_at_all() {
    let forwarded = gated(&primary_stream());

    // Rendered content still crosses — the tee is control-silent, not silent.
    assert!(forwarded.contains("Compiling ratty"));
    assert!(forwarded.contains("ratty:flash;color=green"));

    for leak in [
        "user.join",
        "user.cursor",
        "user.renew",
        "user.leave",
        "note.remove",
        SOURCE_ID,
        SOURCE_NOTE_ID,
        "unclassifiable", // the malformed command's payload
        "778",            // the query plane is never rendered
        "tok1",
    ] {
        assert!(
            !forwarded.contains(leak),
            "{leak:?} reached a spectator through the transport"
        );
    }
}

#[test]
fn only_fresh_rows_are_synthesized_and_only_under_mirrored_ids() {
    let rows = walk();
    assert_eq!(rows.len(), 2, "the expired row must not survive the walk");

    let mut mirror = Mirror::new();
    let cmds = mirror.apply_walk(&rows, Instant::now(), Instant::now(), true);
    let wire: String = cmds.iter().map(Cmd::to_wire).collect();

    // The identity key is rewritten: a caller-local id under the spectator's
    // own namespace 0 would name a different participant.
    assert!(wire.contains(&format!("id=r0.{SOURCE_ID}")), "{wire:?}");
    assert!(
        wire.contains(&format!("id=r0.{SOURCE_NOTE_ID}")),
        "{wire:?}"
    );
    assert!(
        !wire.contains(&format!("id={SOURCE_ID}")),
        "a source id reached a spectator verbatim: {wire:?}"
    );
    assert!(
        !wire.contains(&format!("id={SOURCE_NOTE_ID}")),
        "a source id reached a spectator verbatim: {wire:?}"
    );

    // The expired row is absent in every form.
    assert!(!wire.contains("already-expired"), "{wire:?}");
    assert!(!wire.contains("Bo"), "{wire:?}");

    // Rendered content is public by definition and does cross.
    assert!(wire.contains("Ann%20W"), "{wire:?}");
    assert!(wire.contains("review%20this"), "{wire:?}");
    assert!(wire.contains("%2300ffcc"), "{wire:?}");

    // Every synthesized command is one the terminal's own parser accepts as
    // presence-control, and every join is a revision-continuing replace.
    let mut joins = 0;
    for cmd in &cmds {
        let sequence = cmd.to_wire();
        let body = sequence
            .strip_prefix("\x1b]777;")
            .and_then(|rest| rest.strip_suffix('\x07'))
            .expect("BEL-terminated 777");
        let parsed = osc::parse_control(body)
            .and_then(|control| control.command)
            .unwrap_or_else(|| panic!("the crate parser rejected {body:?}"));
        assert!(parsed.is_presence_control(), "{body:?}");
        match parsed {
            osc::RattyAiCommand::UserJoin { id, replace, .. }
            | osc::RattyAiCommand::Note { id, replace, .. } => {
                joins += 1;
                assert!(replace, "{body:?} must carry replace=true");
                assert!(id.len() <= MAX_MIRROR_ID_BYTES, "{id} exceeds the id cap");
            }
            _ => {}
        }
    }
    assert_eq!(joins, 2, "one join and one note: {cmds:?}");
}

#[test]
fn the_two_halves_never_overlap() {
    // Whatever the mirror synthesizes, the transport contributed none of it:
    // the gated stream contains no presence action at all, so a spectator's
    // entire presence picture is the mirror's — and the mirror only ever
    // states rows that were public when it stated them.
    let forwarded = gated(&primary_stream());
    let synthesized: String = Mirror::new()
        .apply_walk(&walk(), Instant::now(), Instant::now(), true)
        .iter()
        .map(Cmd::to_wire)
        .collect();

    assert!(!synthesized.is_empty());
    for action in [
        "user.join",
        "user.cursor",
        "note;",
        "user.leave",
        "note.remove",
    ] {
        assert!(
            !forwarded.contains(action),
            "{action:?} crossed on the transport"
        );
    }
    assert!(synthesized.contains("user.join"));
    assert!(synthesized.contains("note;"));
}
