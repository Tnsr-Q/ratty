//! Control frames — the out-of-band JSON metadata channel.
//!
//! Binary WebSocket frames carry gated output bytes; JSON text frames carry
//! everything structured (`docs/research/relay-design.md`, "Fan-out wire").
//! Structured context never travels inline in the byte stream.

use serde::{Deserialize, Serialize};

use crate::presence::MirrorId;

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
    /// Which mirrored presence rows this spectator now holds (stage 2).
    ///
    /// Spectator instances are **connection-ephemeral**: a spectator tracks
    /// the mirrored ids it has applied and synthesizes `user.leave` /
    /// `note.remove` for each on **any** socket close — clean `end` or drop —
    /// so mirrors tear down with the connection and survive every relay-side
    /// failure mode (crash, stall, drop-on-overflow). This frame is how the
    /// set is announced: structured context out-of-band, never inline in the
    /// byte stream (the #25 doctrine), and never a second OSC parser on the
    /// spectator side.
    ///
    /// `add` is sent **before** the bytes that join those rows and `remove`
    /// **after** the bytes that clear them, so a spectator's tracked set is
    /// always a superset of what it actually applied — an over-count costs
    /// only `unknown-id` noise in its own error ring, while an under-count
    /// would leave a mirrored row standing.
    PresenceMirror {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        add: Vec<MirrorId>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        remove: Vec<MirrorId>,
        /// The primary reset: every spectator roster was cleared by the
        /// forwarded bytes, so the tracked set drops with them.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        clear: bool,
    },
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
            Control::PresenceMirror {
                add: vec![MirrorId::participant("r0.ann")],
                remove: vec![MirrorId::note("r0.n1")],
                clear: false,
            },
            Control::PresenceMirror {
                add: Vec::new(),
                remove: Vec::new(),
                clear: true,
            },
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
    fn mirror_ids_name_their_roster() {
        let json = Control::PresenceMirror {
            add: vec![MirrorId::participant("r0.ann")],
            remove: Vec::new(),
            clear: false,
        }
        .to_json();
        assert!(json.contains("\"presence-mirror\""), "{json}");
        assert!(json.contains("\"kind\":\"participant\""), "{json}");
        assert!(json.contains("\"r0.ann\""), "{json}");
        // Empty halves stay off the wire entirely.
        assert!(!json.contains("remove"), "{json}");
        assert!(!json.contains("clear"), "{json}");
    }

    #[test]
    fn tag_is_kebab_case() {
        let json = Control::SnapshotBegin.to_json();
        assert!(json.contains("\"snapshot-begin\""), "{json}");
    }
}
