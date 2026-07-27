//! The stage-1 required test (`docs/research/relay-design.md`, "What #46
//! builds"): the gate's tokenization must not depend on chunk boundaries —
//! PTY reads split sequences arbitrarily, and a divergence here is
//! precisely a presence leak. The corpus runs whole, then under many
//! deterministic pseudo-random splits, and every split must produce the
//! identical gated output; classification is additionally checked against
//! the crate's own `parse_control` on each embedded 777.

#[allow(dead_code)] // The gate's injection-boundary half belongs to the host.
#[path = "../src/gate.rs"]
mod gate;
#[allow(dead_code)]
#[path = "../../../src/osc.rs"]
mod osc;

use gate::{Gate, GateStats, Seg};

/// Deterministic LCG so splits are reproducible without wall-clock or OS
/// randomness.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self, bound: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 33) as usize) % bound
    }
}

fn corpus() -> Vec<u8> {
    let mut c = Vec::new();
    c.extend_from_slice(b"login: demo\r\nhello \x1b[1;32mworld\x1b[0m\r\n");
    // Rendered 777s (forwarded), one with a token (stripped).
    c.extend_from_slice(b"\x1b]777;ratty:flash;color=green\x07");
    c.extend_from_slice(b"\x1b]777;ratty:flash;color=red&tok=deadbeef\x07");
    // The whole presence family, well-formed (excised by classification).
    c.extend_from_slice(b"\x1b]777;ratty:user.join;id=ann&name=Ann&color=magenta\x07");
    c.extend_from_slice(b"\x1b]777;ratty:user.cursor;id=ann&x=3&y=4\x07");
    c.extend_from_slice(b"\x1b]777;ratty:note;id=n1&text=secret&x=2&y=6&ttl=30\x07");
    c.extend_from_slice(b"\x1b]777;ratty:user.renew;id=ann\x07");
    c.extend_from_slice(b"\x1b]777;ratty:note.remove;id=n1\x07");
    c.extend_from_slice(b"\x1b]777;ratty:user.leave;id=ann\x07");
    // Macro/reactive/execution control, well-formed (excised).
    c.extend_from_slice(b"\x1b]777;ratty:macro.record;name=m\x07");
    c.extend_from_slice(b"\x1b]777;ratty:rule.set;name=r&sensor=sys.cpu&above=90&do=flash\x07");
    c.extend_from_slice(b"\x1b]777;ratty:avatar.stop\x07");
    // Malformed presence (no x/y): unclassifiable, so withheld by the
    // fail-closed rule — its text must not reach a spectator.
    c.extend_from_slice(b"\x1b]777;ratty:note;id=bad&text=leakme\x07");
    // Query plane, both terminators (excised).
    c.extend_from_slice(b"\x1b]778;q;tok=abc123;op=caps\x07");
    c.extend_from_slice(b"\x1b]778;r;tok=abc123;ok=1;data=e30\x1b\\");
    // Non-ratty 777 (forwarded verbatim).
    c.extend_from_slice(b"\x1b]777;notify;Build;done\x07");
    // APC (kitty/RGP-style, forwarded live).
    c.extend_from_slice(b"\x1b_Gf=100,a=T;QUJDRA==\x1b\\");
    // Mid-stream full clear (anchor) and a reset command (anchor).
    c.extend_from_slice(b"before-clear\x1b[2Jafter-clear");
    c.extend_from_slice(b"\x1b]777;ratty:reset\x07");
    // ST-terminated rendered OSC (title set), plus scrollback-only clear.
    c.extend_from_slice(b"\x1b]0;title\x1b\\\x1b[3J");
    c.extend_from_slice(b"tail text\r\n");
    c
}

fn run_split(corpus: &[u8], splits: &[usize]) -> (Vec<Seg>, GateStats) {
    let mut gate = Gate::new();
    let mut segs: Vec<Seg> = Vec::new();
    let mut feed = |chunk: &[u8]| {
        for seg in gate.push(chunk) {
            // Coalesce adjacent Data runs so segmentation differences
            // (which are chunk-driven by design) do not fail equality;
            // the BYTES and the anchor positions must be identical.
            match (segs.last_mut(), seg) {
                (Some(Seg::Data(tail)), Seg::Data(new)) => tail.extend_from_slice(&new),
                (_, seg) => segs.push(seg),
            }
        }
    };
    let mut start = 0;
    for &split in splits {
        feed(&corpus[start..split]);
        start = split;
    }
    feed(&corpus[start..]);
    (segs, gate.stats)
}

#[test]
fn chunk_splits_never_change_the_gated_stream() {
    let corpus = corpus();
    let (reference, ref_stats) = run_split(&corpus, &[]);

    let mut rng = Lcg(0x5EED_CAFE);
    for round in 0..200 {
        let mut splits: Vec<usize> = Vec::new();
        let n_splits = 1 + rng.next(24);
        for _ in 0..n_splits {
            splits.push(1 + rng.next(corpus.len() - 1));
        }
        splits.sort_unstable();
        splits.dedup();
        let (segs, stats) = run_split(&corpus, &splits);
        assert_eq!(segs, reference, "divergence at round {round}: {splits:?}");
        assert_eq!(stats, ref_stats, "stats divergence at round {round}");
    }

    // Byte-at-a-time is the pathological split.
    let every_byte: Vec<usize> = (1..corpus.len()).collect();
    let (segs, stats) = run_split(&corpus, &every_byte);
    assert_eq!(segs, reference);
    assert_eq!(stats, ref_stats);
}

#[test]
fn classification_agrees_with_the_crate_parser() {
    // Every `ratty:` 777 embedded in the corpus, classified directly with
    // the crate's own predicates; the gate must excise exactly the
    // control-plane/execution-control set.
    let corpus = corpus();
    let (segs, stats) = run_split(&corpus, &[]);
    let forwarded: Vec<u8> = segs
        .iter()
        .flat_map(|seg| match seg {
            Seg::Data(b) | Seg::Anchor { bytes: b, .. } => b.clone(),
        })
        .collect();
    let forwarded_text = String::from_utf8_lossy(&forwarded).into_owned();

    // Two independent excision paths, counted separately so neither can
    // silently absorb the other's misses.
    let mut expected_classified = 0u64;
    let mut expected_fail_closed = 0u64;
    for seq in extract_777_payloads(&corpus) {
        let action = seq.split(';').next().unwrap();
        match osc::parse_control(&seq).and_then(|control| control.command) {
            Some(cmd) if cmd.is_control_plane() || cmd.is_execution_control() => {
                expected_classified += 1;
                assert!(
                    !forwarded_text.contains(action),
                    "excised action {action} leaked into the gated stream"
                );
            }
            // Unparseable `ratty:` bytes are withheld regardless.
            None => expected_fail_closed += 1,
            Some(_) => {}
        }
    }
    assert!(
        expected_classified >= 8 && expected_fail_closed >= 1,
        "corpus must exercise both paths (classified {expected_classified}, fail-closed {expected_fail_closed})"
    );
    assert!(
        !forwarded_text.contains("leakme"),
        "a malformed presence command's text reached the gated stream"
    );
    assert_eq!(
        stats.excised_control,
        expected_classified + expected_fail_closed
    );
    assert_eq!(stats.excised_778, 2);
    assert_eq!(stats.stripped_tok, 1);
    assert!(
        !forwarded_text.contains("tok="),
        "a correlation token leaked into the gated stream"
    );
}

/// The `ratty:`-prefixed data strings of every 777 in the corpus.
fn extract_777_payloads(corpus: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(corpus);
    let mut out = Vec::new();
    for chunk in text.split("\x1b]777;").skip(1) {
        let end = chunk
            .find('\x07')
            .or_else(|| chunk.find("\x1b\\"))
            .expect("corpus sequences are terminated");
        let data = &chunk[..end];
        if data.starts_with("ratty:") {
            out.push(data.to_string());
        }
    }
    out
}
