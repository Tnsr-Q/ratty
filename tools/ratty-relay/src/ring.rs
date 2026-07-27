//! The anchored replay ring.
//!
//! Not a rolling window: replaying a cap-truncated stream from a non-clear
//! anchor is exactly the mid-sequence tear the design forbids. The ring
//! accumulates gated bytes from the last full-clear anchor (777 `reset`,
//! ED2, RIS); if a session scrolls past the cap without ever clearing, the
//! ring empties and goes *degraded* — late joiners then get a synthetic
//! blank anchor (ED2 + cursor home) and the live tail only, disclosed via
//! the `degraded` flag in `hello`.

use crate::gate::Seg;

/// Synthetic anchor for the degraded path: clear + cursor home.
pub const SYNTHETIC_ANCHOR: &[u8] = b"\x1b[2J\x1b[H";

pub struct Ring {
    cap: usize,
    buf: Vec<u8>,
    degraded: bool,
}

impl Ring {
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            // A fresh session starts at a known-clear screen: the empty
            // ring is a valid anchor, not a degraded state.
            buf: Vec::new(),
            degraded: false,
        }
    }

    /// Apply one gated segment.
    pub fn push(&mut self, seg: &Seg) {
        match seg {
            Seg::Anchor { bytes } => {
                self.buf.clear();
                self.buf.extend_from_slice(bytes);
                self.degraded = false;
            }
            Seg::Data(bytes) => {
                if self.degraded {
                    return;
                }
                if self.buf.len() + bytes.len() > self.cap {
                    self.buf.clear();
                    self.degraded = true;
                } else {
                    self.buf.extend_from_slice(bytes);
                }
            }
        }
    }

    pub fn degraded(&self) -> bool {
        self.degraded
    }

    /// The late-join snapshot bytes: the anchored history, or the synthetic
    /// blank anchor when degraded.
    pub fn snapshot(&self) -> Vec<u8> {
        if self.degraded {
            SYNTHETIC_ANCHOR.to_vec()
        } else {
            self.buf.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accumulates_until_cap_then_degrades() {
        let mut ring = Ring::new(8);
        ring.push(&Seg::Data(b"12345".to_vec()));
        assert!(!ring.degraded());
        assert_eq!(ring.snapshot(), b"12345".to_vec());
        ring.push(&Seg::Data(b"6789".to_vec())); // 9 > 8
        assert!(ring.degraded());
        assert_eq!(ring.snapshot(), SYNTHETIC_ANCHOR.to_vec());
        // Further data stays dropped while degraded.
        ring.push(&Seg::Data(b"x".to_vec()));
        assert_eq!(ring.snapshot(), SYNTHETIC_ANCHOR.to_vec());
    }

    #[test]
    fn anchor_rearms_a_degraded_ring() {
        // Cap must exceed the anchor plus the follow-up data, or the
        // re-armed ring degrades again on the very next push.
        let mut ring = Ring::new(16);
        ring.push(&Seg::Data(vec![b'x'; 17]));
        assert!(ring.degraded());
        ring.push(&Seg::Anchor {
            bytes: b"\x1b[2J".to_vec(),
        });
        assert!(!ring.degraded());
        ring.push(&Seg::Data(b"ok".to_vec()));
        assert_eq!(ring.snapshot(), b"\x1b[2Jok".to_vec());
    }

    #[test]
    fn anchor_replaces_history() {
        let mut ring = Ring::new(64);
        ring.push(&Seg::Data(b"old".to_vec()));
        ring.push(&Seg::Anchor {
            bytes: b"\x1bc".to_vec(),
        });
        ring.push(&Seg::Data(b"new".to_vec()));
        assert_eq!(ring.snapshot(), b"\x1bcnew".to_vec());
    }
}
