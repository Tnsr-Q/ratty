//! The control-silent gate — a streaming OSC/APC boundary scanner.
//!
//! PTY reads arrive as arbitrary 16 KiB chunks with no sequence alignment
//! (src/runtime.rs reader thread), presence commands are BEL-terminated and
//! 778 replies ST-terminated, so the tee recognizes both terminators,
//! withholds an OSC sequence until it classifies, and forwards everything
//! else untouched. Classification calls the crate's own predicates through
//! the `#[path]`-shared `osc` module — never a re-typed command list.
//!
//! Excised from the teed copy (the primary always receives every byte
//! verbatim upstream of this gate):
//! - OSC 777 commands in the control-plane or execution-control classes
//!   (`is_control_plane` composes the macro/reactive/presence families;
//!   `is_execution_control` covers `avatar.stop`/`avatar.cancel`);
//! - every OSC 778 sequence (request plane, never rendered);
//! - the `tok=` correlation param on any forwarded 777.
//!
//! An OSC payload larger than the crate's own sequence bound is excised
//! whole: the terminal drops such sequences too, so nothing rendered is
//! lost, and never-forward-unclassifiable is the conservative rule.

use crate::osc;

/// Mirrors the crate-side `MAX_OSC_SEQUENCE_BYTES` (src/inline.rs — 64 KiB,
/// `pub(crate)` so not reachable through the shared module). A sequence the
/// terminal itself would refuse to accumulate never needs forwarding.
const MAX_OSC_PAYLOAD: usize = 64 * 1024;

/// How many CSI parameter bytes we keep for the ED2 anchor check.
const MAX_CSI_PARAMS: usize = 16;

/// What a classified sequence did to the stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seg {
    /// Gated bytes to forward to spectators.
    Data(Vec<u8>),
    /// A full-clear anchor (777 `reset`, CSI `2J`, or RIS). `bytes` is the
    /// forwarded sequence itself so the ring can restart exactly at it.
    Anchor { bytes: Vec<u8> },
}

/// Running excision counters, for the host status line and tests.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct GateStats {
    pub excised_control: u64,
    pub excised_778: u64,
    pub stripped_tok: u64,
    pub excised_overflow: u64,
}

enum State {
    Ground,
    /// Saw ESC; deciding what follows.
    Esc,
    /// Inside `ESC ]` — payload withheld until the terminator classifies it.
    Osc {
        payload: Vec<u8>,
        esc_pending: bool,
        overflow: bool,
    },
    /// Inside a live-forwarded ST-terminated string (APC/DCS/SOS/PM).
    PassSt {
        esc_pending: bool,
    },
    /// Inside `ESC [`. Buffered whole (not forwarded byte-by-byte) so the
    /// ED2 anchor check cannot depend on chunk boundaries: a CSI split
    /// across two PTY reads would otherwise have its prefix already
    /// flushed into a `Data` segment by the time the final byte arrives.
    Csi {
        seq: Vec<u8>,
        params: Vec<u8>,
    },
}

pub struct Gate {
    state: State,
    /// The current run of forwarded bytes, flushed into a `Seg::Data`.
    run: Vec<u8>,
    out: Vec<Seg>,
    pub stats: GateStats,
}

impl Default for Gate {
    fn default() -> Self {
        Self::new()
    }
}

impl Gate {
    pub fn new() -> Self {
        Self {
            state: State::Ground,
            run: Vec::new(),
            out: Vec::new(),
            stats: GateStats::default(),
        }
    }

    /// Feed one PTY chunk; returns the gated segments it completed. Chunk
    /// boundaries never affect the output (the differential test pins this).
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Seg> {
        for &byte in chunk {
            self.step(byte);
        }
        self.flush_run();
        std::mem::take(&mut self.out)
    }

    fn step(&mut self, byte: u8) {
        match &mut self.state {
            State::Ground => match byte {
                0x1b => self.state = State::Esc,
                _ => self.run.push(byte),
            },
            State::Esc => match byte {
                b']' => {
                    self.state = State::Osc {
                        payload: Vec::new(),
                        esc_pending: false,
                        overflow: false,
                    }
                }
                b'[' => {
                    self.state = State::Csi {
                        seq: vec![0x1b, b'['],
                        params: Vec::new(),
                    };
                }
                b'_' | b'P' | b'X' | b'^' => {
                    self.run.extend_from_slice(&[0x1b, byte]);
                    self.state = State::PassSt { esc_pending: false };
                }
                b'c' => {
                    // RIS — hard terminal reset: forward and anchor.
                    self.emit_anchor(vec![0x1b, b'c']);
                    self.state = State::Ground;
                }
                _ => {
                    self.run.extend_from_slice(&[0x1b, byte]);
                    self.state = State::Ground;
                }
            },
            State::Osc {
                payload,
                esc_pending,
                overflow,
            } => {
                if *esc_pending {
                    if byte == b'\\' {
                        // ST terminator.
                        let payload = std::mem::take(payload);
                        let overflow = *overflow;
                        self.finish_osc(payload, b"\x1b\\", overflow);
                    } else {
                        // vte semantics: a bare ESC terminates the OSC and
                        // the ESC + this byte reprocess as a new sequence.
                        let payload = std::mem::take(payload);
                        let overflow = *overflow;
                        self.finish_osc(payload, b"", overflow);
                        self.step(0x1b);
                        self.step(byte);
                    }
                    return;
                }
                match byte {
                    0x07 => {
                        let payload = std::mem::take(payload);
                        let overflow = *overflow;
                        self.finish_osc(payload, b"\x07", overflow);
                    }
                    0x1b => *esc_pending = true,
                    _ => {
                        if payload.len() >= MAX_OSC_PAYLOAD {
                            *overflow = true;
                        } else {
                            payload.push(byte);
                        }
                    }
                }
            }
            State::PassSt { esc_pending } => {
                if *esc_pending {
                    if byte == b'\\' {
                        self.run.extend_from_slice(&[0x1b, b'\\']);
                        self.state = State::Ground;
                    } else {
                        self.run.push(0x1b);
                        self.state = State::Ground;
                        self.step(byte);
                    }
                    return;
                }
                match byte {
                    0x1b => *esc_pending = true,
                    _ => self.run.push(byte),
                }
            }
            State::Csi { seq, params } => {
                seq.push(byte);
                match byte {
                    0x30..=0x3f => {
                        if params.len() < MAX_CSI_PARAMS {
                            params.push(byte);
                        }
                    }
                    0x20..=0x2f => {} // intermediates
                    0x40..=0x7e => {
                        // Final byte: CSI 2 J is the full-clear anchor.
                        let is_ed2 = byte == b'J' && params.as_slice() == b"2";
                        let seq = std::mem::take(seq);
                        self.state = State::Ground;
                        if is_ed2 {
                            self.emit_anchor(seq);
                        } else {
                            self.run.extend_from_slice(&seq);
                        }
                    }
                    _ => {
                        // Not a valid CSI byte; forward what we buffered and
                        // fall back to ground (a tee never interprets CSI
                        // beyond the ED2 anchor check).
                        let seq = std::mem::take(seq);
                        self.state = State::Ground;
                        self.run.extend_from_slice(&seq);
                    }
                }
            }
        }
    }

    /// A terminated OSC: classify and forward, strip, excise, or anchor.
    fn finish_osc(&mut self, payload: Vec<u8>, terminator: &[u8], overflow: bool) {
        self.state = State::Ground;
        if overflow {
            self.stats.excised_overflow += 1;
            return;
        }
        match classify(&payload) {
            Verdict::Forward => {
                self.run.extend_from_slice(&[0x1b, b']']);
                self.run.extend_from_slice(&payload);
                self.run.extend_from_slice(terminator);
            }
            Verdict::ForwardStripped(stripped) => {
                self.stats.stripped_tok += 1;
                self.run.extend_from_slice(&[0x1b, b']']);
                self.run.extend_from_slice(&stripped);
                self.run.extend_from_slice(terminator);
            }
            Verdict::Anchor => {
                let mut bytes = vec![0x1b, b']'];
                bytes.extend_from_slice(&payload);
                bytes.extend_from_slice(terminator);
                self.emit_anchor(bytes);
            }
            Verdict::ExciseControl => self.stats.excised_control += 1,
            Verdict::Excise778 => self.stats.excised_778 += 1,
        }
    }

    fn emit_anchor(&mut self, bytes: Vec<u8>) {
        self.flush_run();
        self.out.push(Seg::Anchor { bytes });
    }

    fn flush_run(&mut self) {
        if !self.run.is_empty() {
            self.out.push(Seg::Data(std::mem::take(&mut self.run)));
        }
    }
}

enum Verdict {
    Forward,
    ForwardStripped(Vec<u8>),
    Anchor,
    ExciseControl,
    Excise778,
}

/// Classify one complete OSC payload (the bytes between `ESC ]` and the
/// terminator). Only ratty's 777/778 are interpreted; everything else
/// forwards verbatim — the spectator's parser treats it exactly as the
/// primary's did.
fn classify(payload: &[u8]) -> Verdict {
    let Ok(text) = std::str::from_utf8(payload) else {
        // Non-UTF-8 OSC: the terminal's own dispatch cannot parse it either;
        // both sides drop it identically, so forwarding is behavior-equal.
        return Verdict::Forward;
    };
    let Some((number, data)) = text.split_once(';') else {
        return Verdict::Forward;
    };
    match number {
        "778" => Verdict::Excise778,
        "777" => classify_777(data),
        _ => Verdict::Forward,
    }
}

fn classify_777(data: &str) -> Verdict {
    if !data.starts_with(osc::RATTY_AI_PREFIX) {
        // A non-ratty 777 user (e.g. desktop notifications): rendered
        // behavior, not ours to interpret.
        return Verdict::Forward;
    }
    match osc::parse_control(data).and_then(|control| control.command) {
        Some(cmd) if cmd.is_control_plane() || cmd.is_execution_control() => Verdict::ExciseControl,
        Some(osc::RattyAiCommand::Reset) => Verdict::Anchor,
        // A rendered command: forward, less the `tok=` correlation param
        // (envelope metadata for the ack opt-in — never rendered).
        Some(_) => match strip_tok(data) {
            Some(stripped) => {
                let mut bytes = b"777;".to_vec();
                bytes.extend_from_slice(stripped.as_bytes());
                Verdict::ForwardStripped(bytes)
            }
            None => Verdict::Forward,
        },
        // **Fail closed.** A `ratty:` command that does not parse is
        // unclassifiable, and an unclassifiable control byte is exactly
        // what this gate exists to withhold: a malformed `note` still
        // carries its text, and forwarding it would hand a spectator the
        // payload off the wire even though their terminal rejects the
        // command. Nothing rendered is lost — the primary's own parser
        // rejects it identically (`src/ai.rs` bad-command path).
        None => Verdict::ExciseControl,
    }
}

/// Remove the `tok=` param from a `ratty:` command string, preserving every
/// other byte. Params split on `&` exactly as the crate's `Payload::parse`
/// does, so the strip can never disagree with crate tokenization. Returns
/// `None` when there is no `tok=` (forward the original bytes untouched).
fn strip_tok(data: &str) -> Option<String> {
    let (action, payload) = match data.split_once(';') {
        Some((action, payload)) => (action, payload),
        None => return None,
    };
    if !payload
        .split('&')
        .any(|token| token.split('=').next() == Some(osc::ACK_TOKEN_KEY))
    {
        return None;
    }
    let kept: Vec<&str> = payload
        .split('&')
        .filter(|token| !token.is_empty() && token.split('=').next() != Some(osc::ACK_TOKEN_KEY))
        .collect();
    if kept.is_empty() {
        Some(action.to_string())
    } else {
        Some(format!("{action};{}", kept.join("&")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate_all(chunks: &[&[u8]]) -> (Vec<Seg>, GateStats) {
        let mut gate = Gate::new();
        let mut segs = Vec::new();
        for chunk in chunks {
            segs.extend(gate.push(chunk));
        }
        (segs, gate.stats)
    }

    fn flat_data(segs: &[Seg]) -> Vec<u8> {
        let mut out = Vec::new();
        for seg in segs {
            match seg {
                Seg::Data(bytes) | Seg::Anchor { bytes } => out.extend_from_slice(bytes),
            }
        }
        out
    }

    #[test]
    fn plain_text_passes_verbatim() {
        let input = b"hello \x1b[1;32mworld\x1b[0m\r\n";
        let (segs, stats) = gate_all(&[input]);
        assert_eq!(flat_data(&segs), input.to_vec());
        assert_eq!(stats, GateStats::default());
    }

    #[test]
    fn presence_family_is_excised() {
        // Well-formed commands: excised by classification, not by the
        // fail-closed backstop (`malformed_ratty_commands_fail_closed`).
        for cmd in [
            "user.join;id=a&name=Ann",
            "user.renew;id=a",
            "user.cursor;id=a&x=1&y=2",
            "user.leave;id=a",
            "note;id=n&text=hi&x=1&y=2",
            "note.remove;id=n",
        ] {
            assert!(
                osc::parse_control(&format!("ratty:{cmd}"))
                    .and_then(|c| c.command)
                    .is_some_and(|c| c.is_presence_control()),
                "{cmd} must be a well-formed presence command for this test to mean anything"
            );
            let seq = format!("\x1b]777;ratty:{cmd}\x07");
            let (segs, stats) = gate_all(&[seq.as_bytes()]);
            assert!(flat_data(&segs).is_empty(), "{cmd} leaked");
            assert_eq!(stats.excised_control, 1, "{cmd}");
        }
    }

    #[test]
    fn macro_reactive_and_execution_control_are_excised() {
        for cmd in [
            "macro.record;name=m",
            "macro.stop",
            "macro.play;name=m",
            "rule.set;name=r&sensor=sys.cpu&above=90&do=flash",
            "rule.remove;name=r",
            "sensor.publish;name=agent.x&value=1",
            "sensor.remove;name=s",
            "avatar.stop",
            "avatar.cancel;id=x",
        ] {
            let parsed = osc::parse_control(&format!("ratty:{cmd}")).and_then(|c| c.command);
            assert!(
                parsed
                    .as_ref()
                    .is_some_and(|c| c.is_control_plane() || c.is_execution_control()),
                "{cmd} must be a well-formed control command for this test to mean anything"
            );
            let seq = format!("\x1b]777;ratty:{cmd}\x07");
            let (segs, stats) = gate_all(&[seq.as_bytes()]);
            assert!(flat_data(&segs).is_empty(), "{cmd} leaked");
            assert_eq!(
                stats.excised_control, 1,
                "{cmd} should classify control-plane/execution-control"
            );
        }
    }

    #[test]
    fn malformed_ratty_commands_fail_closed() {
        // A `note` whose coordinates do not parse is not a valid command —
        // but it still carries its text. Forwarding it would leak that
        // text to spectators off the wire even though their terminal
        // rejects the command, so unclassifiable `ratty:` bytes are
        // withheld.
        let seq = b"\x1b]777;ratty:note;id=n&text=confidential\x07";
        assert!(
            osc::parse_control("ratty:note;id=n&text=confidential")
                .and_then(|c| c.command)
                .is_none(),
            "this payload must be unparseable for the test to exercise fail-closed"
        );
        let (segs, stats) = gate_all(&[seq]);
        assert!(flat_data(&segs).is_empty(), "malformed note leaked");
        assert_eq!(stats.excised_control, 1);

        // Same rule for an entirely unknown action.
        let (segs, stats) = gate_all(&[b"\x1b]777;ratty:not.a.real.action;x=1\x07"]);
        assert!(flat_data(&segs).is_empty());
        assert_eq!(stats.excised_control, 1);
    }

    #[test]
    fn rendered_commands_forward_and_reset_anchors() {
        let flash = b"\x1b]777;ratty:flash;color=green\x07";
        let (segs, stats) = gate_all(&[flash]);
        assert_eq!(flat_data(&segs), flash.to_vec());
        assert_eq!(stats, GateStats::default());

        let reset = b"\x1b]777;ratty:reset\x07";
        let (segs, _) = gate_all(&[reset]);
        assert_eq!(
            segs,
            vec![Seg::Anchor {
                bytes: reset.to_vec()
            }]
        );
    }

    #[test]
    fn osc_778_is_excised_under_both_terminators() {
        let bel = b"\x1b]778;q;tok=abc;op=caps\x07";
        let st = b"\x1b]778;r;tok=abc;ok=1\x1b\\";
        let (segs, stats) = gate_all(&[bel, st]);
        assert!(flat_data(&segs).is_empty());
        assert_eq!(stats.excised_778, 2);
    }

    #[test]
    fn tok_is_stripped_from_forwarded_777() {
        let seq = b"\x1b]777;ratty:flash;color=red&tok=abc123\x07";
        let (segs, stats) = gate_all(&[seq]);
        assert_eq!(
            flat_data(&segs),
            b"\x1b]777;ratty:flash;color=red\x07".to_vec()
        );
        assert_eq!(stats.stripped_tok, 1);

        // tok as the only param drops the payload separator too.
        let seq = b"\x1b]777;ratty:flash;tok=abc\x07";
        let (segs, _) = gate_all(&[seq]);
        assert_eq!(flat_data(&segs), b"\x1b]777;ratty:flash\x07".to_vec());
    }

    #[test]
    fn non_ratty_777_forwards_verbatim() {
        let seq = b"\x1b]777;notify;Title;Body\x07";
        let (segs, stats) = gate_all(&[seq]);
        assert_eq!(flat_data(&segs), seq.to_vec());
        assert_eq!(stats, GateStats::default());
    }

    #[test]
    fn ed2_and_ris_anchor() {
        let (segs, _) = gate_all(&[b"before\x1b[2Jafter"]);
        assert_eq!(
            segs,
            vec![
                Seg::Data(b"before".to_vec()),
                Seg::Anchor {
                    bytes: b"\x1b[2J".to_vec()
                },
                Seg::Data(b"after".to_vec()),
            ]
        );
        // 3J (scrollback clear) is not an anchor.
        let (segs, _) = gate_all(&[b"\x1b[3J"]);
        assert_eq!(segs, vec![Seg::Data(b"\x1b[3J".to_vec())]);
        let (segs, _) = gate_all(&[b"\x1bc"]);
        assert_eq!(
            segs,
            vec![Seg::Anchor {
                bytes: b"\x1bc".to_vec()
            }]
        );
    }

    #[test]
    fn apc_streams_through() {
        // Kitty/RGP ride APC; the gate forwards them live, ST-terminated.
        let seq = b"\x1b_Gf=100,a=T;YWJj\x1b\\tail";
        let (segs, _) = gate_all(&[seq]);
        assert_eq!(flat_data(&segs), seq.to_vec());
    }

    #[test]
    fn bare_esc_inside_osc_reprocesses() {
        // ESC + non-backslash terminates the OSC (vte semantics) and the
        // ESC starts a new sequence.
        let input = b"\x1b]777;ratty:flash\x1b[2J";
        let (segs, _) = gate_all(&[input]);
        assert_eq!(
            segs,
            vec![
                Seg::Data(b"\x1b]777;ratty:flash".to_vec()),
                Seg::Anchor {
                    bytes: b"\x1b[2J".to_vec()
                },
            ]
        );
    }

    #[test]
    fn oversize_osc_is_excised_whole() {
        let mut seq = b"\x1b]777;ratty:flash;color=".to_vec();
        seq.extend(std::iter::repeat_n(b'x', MAX_OSC_PAYLOAD + 10));
        seq.push(0x07);
        let (segs, stats) = gate_all(&[&seq]);
        assert!(flat_data(&segs).is_empty());
        assert_eq!(stats.excised_overflow, 1);
    }
}
