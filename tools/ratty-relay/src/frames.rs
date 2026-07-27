//! Control frames — the out-of-band JSON metadata channel.
//!
//! Binary WebSocket frames carry gated output bytes; JSON text frames carry
//! everything structured (`docs/research/relay-design.md`, "Fan-out wire").
//! Structured context never travels inline in the byte stream.

use serde::{Deserialize, Serialize};

/// One control frame. `type` is the tag, kebab-case on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Control {
    /// First frame every spectator receives. Geometry is the primary's; a
    /// spectator letterboxes or warns — the byte stream encodes this grid.
    /// `degraded` means the snapshot that follows starts from a synthetic
    /// blank anchor (ED2 + cursor home), not a replayed clear.
    Hello {
        session: String,
        cols: u16,
        rows: u16,
        seq: u64,
        degraded: bool,
    },
    /// The primary's grid changed (SIGWINCH mirrored inward).
    Resize {
        cols: u16,
        rows: u16,
    },
    /// The primary reset; spectators already reset in lockstep via the teed
    /// bytes — this frame is advisory metadata.
    ResetNotice,
    /// Brackets the late-join snapshot: live frames are held while the
    /// snapshot is queued and resume after `snapshot-end`.
    SnapshotBegin,
    SnapshotEnd,
    /// The session is over; the socket closes after this frame.
    End {
        reason: String,
    },
}

impl Control {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("control frames serialize")
    }

    pub fn from_json(text: &str) -> Option<Self> {
        serde_json::from_str(text).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_frames_round_trip() {
        let frames = [
            Control::Hello {
                session: "demo".into(),
                cols: 80,
                rows: 24,
                seq: 7,
                degraded: false,
            },
            Control::Resize {
                cols: 120,
                rows: 40,
            },
            Control::ResetNotice,
            Control::SnapshotBegin,
            Control::SnapshotEnd,
            Control::End {
                reason: "session-ended".into(),
            },
        ];
        for frame in frames {
            let json = frame.to_json();
            assert_eq!(Control::from_json(&json), Some(frame));
        }
    }

    #[test]
    fn tag_is_kebab_case() {
        let json = Control::SnapshotBegin.to_json();
        assert!(json.contains("\"snapshot-begin\""), "{json}");
    }
}
