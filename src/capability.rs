//! Scene-level capability derivation: the trusted tier of #23.
//!
//! A scene-level capability is authority over shared presentation state
//! that no agent namespace owns: the avatar's representation and global
//! visibility, other agents' queued speech, the scene ambient audio bed.
//! Ordinary wire agents never hold one implicitly — they submit attributed
//! speech and gestures inside their own namespace, and everything above
//! that tier answers `not-permitted`.
//!
//! Authority = ingress principal × out-of-band grant, decided at apply
//! time. The derivation below is pure and total over
//! ([`IngressSource`], [`AppConfig`]): nothing inside a byte stream
//! contributes to the answer, so the wire can never grant a capability to
//! itself. Grants come only from trusted config at load (the embedding
//! page's TOML on wasm) — the same tier as `[audio] allow_scene_ambient`
//! and `[reactive] system_sensors`.
//!
//! Honest limitation, stated plainly: [`IngressSource::Local`] is ONE
//! effective principal — a plain PTY cannot attribute bytes to individual
//! writer processes (see the [`IngressSource`] doc). A grant to `Local` is
//! therefore a grant to *every* process writing this terminal, exactly as
//! `allow_scene_ambient` is today. The match below is deliberately
//! exhaustive with no wildcard arm: when an authenticated transport adds
//! an [`IngressSource`] variant, [`SceneCapability::granted_to`] stops
//! compiling until that transport's scene grants are decided explicitly,
//! out-of-band.

use crate::config::AppConfig;
use crate::runtime::IngressSource;

/// A scene-level capability: one nameable unit of shared-scene authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SceneCapability {
    /// Avatar scene control (#23 §2): change the avatar representation
    /// (`avatar.set`), hide/show it globally (`avatar.hide`/`avatar.show`),
    /// clear another agent's queued speech, or cancel the whole speech
    /// queue. The grant bit lives at `[trust.local] avatar_scene`.
    AvatarScene,
    /// Drive the scene ambient audio bed (`sound.ambient.set`). The grant
    /// bit lives at `[audio] allow_scene_ambient`, unchanged; this variant
    /// routes it through the one capability spine (see the module doc).
    SceneAmbient,
    /// Terminal lifecycle (#49): create, place and close terminals
    /// (`term.spawn` / `term.place` / `term.close`). The grant bit lives at
    /// `[trust.local] terminal_lifecycle`, **default DENY** — unlike
    /// [`Self::AvatarScene`], because these verbs fork processes and
    /// destroy live sessions rather than restyle a shared scene. Holding
    /// it is necessary but not sufficient: the targeted forms additionally
    /// require creator ownership.
    TerminalLifecycle,
    /// Terminal focus (#49): aim the user's keyboard at a terminal
    /// (`term.focus`). The grant bit lives at `[trust.local]
    /// terminal_focus`, **default DENY**, and is deliberately separate
    /// from [`Self::TerminalLifecycle`] (#56 decision 18): focus is the
    /// keystroke-capture primitive, not a convenience verb, so an operator
    /// granting workspace choreography must not be forced to also grant
    /// keystroke redirection — or the reverse.
    TerminalFocus,
}

impl SceneCapability {
    /// Whether trusted config grants this capability to `source`.
    ///
    /// Pure derivation; call it at decision time, next to the ownership
    /// checks. [`AppConfig`] is loaded once at startup and is
    /// wire-immutable, so within a process the answer never changes.
    pub fn granted_to(self, source: IngressSource, config: &AppConfig) -> bool {
        // Grants are per transport CLASS: `Local` remains ONE principal
        // class (see the module doc), so the terminal identity inside the
        // stamp is deliberately ignored here. The designed compile-break
        // still fires on new *variants*.
        match source {
            IngressSource::Local(_) => match self {
                SceneCapability::AvatarScene => config.trust.local.avatar_scene,
                SceneCapability::SceneAmbient => config.audio.allow_scene_ambient,
                SceneCapability::TerminalLifecycle => config.trust.local.terminal_lifecycle,
                SceneCapability::TerminalFocus => config.trust.local.terminal_focus,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derivation_truth_table_for_local() {
        let mut config = AppConfig::default();

        config.trust.local.avatar_scene = true;
        config.audio.allow_scene_ambient = true;
        assert!(SceneCapability::AvatarScene.granted_to(IngressSource::test_boot(), &config));
        assert!(SceneCapability::SceneAmbient.granted_to(IngressSource::test_boot(), &config));

        config.trust.local.avatar_scene = false;
        config.audio.allow_scene_ambient = false;
        assert!(!SceneCapability::AvatarScene.granted_to(IngressSource::test_boot(), &config));
        assert!(!SceneCapability::SceneAmbient.granted_to(IngressSource::test_boot(), &config));

        // The grants are independent bits.
        config.trust.local.avatar_scene = true;
        assert!(SceneCapability::AvatarScene.granted_to(IngressSource::test_boot(), &config));
        assert!(!SceneCapability::SceneAmbient.granted_to(IngressSource::test_boot(), &config));

        // The two terminal grants move independently of each other and of
        // the avatar bit — #56 decision 18's whole point: an operator may
        // grant workspace choreography without granting keystroke
        // redirection, or the reverse.
        config.trust.local.terminal_lifecycle = true;
        assert!(SceneCapability::TerminalLifecycle.granted_to(IngressSource::test_boot(), &config));
        assert!(!SceneCapability::TerminalFocus.granted_to(IngressSource::test_boot(), &config));
        config.trust.local.terminal_lifecycle = false;
        config.trust.local.terminal_focus = true;
        assert!(
            !SceneCapability::TerminalLifecycle.granted_to(IngressSource::test_boot(), &config)
        );
        assert!(SceneCapability::TerminalFocus.granted_to(IngressSource::test_boot(), &config));
    }

    /// Both terminal grants default to DENY — inverting this file's own
    /// local convention (`avatar_scene` defaults granted), because these
    /// verbs fork processes, destroy live sessions, and redirect the
    /// human's keystrokes.
    #[test]
    fn terminal_grants_default_to_denied() {
        let config = AppConfig::default();
        assert!(
            !SceneCapability::TerminalLifecycle.granted_to(IngressSource::test_boot(), &config)
        );
        assert!(!SceneCapability::TerminalFocus.granted_to(IngressSource::test_boot(), &config));
    }

    /// A partial `[trust.local]` table fills the rest from `Default`:
    /// naming one bit must never silently revoke or grant another.
    #[test]
    fn a_partial_trust_table_leaves_the_other_grants_at_their_defaults() {
        let config = AppConfig::from_toml_str("[trust.local]\nterminal_lifecycle = true\n")
            .expect("trust section parses");
        assert!(SceneCapability::TerminalLifecycle.granted_to(IngressSource::test_boot(), &config));
        assert!(
            !SceneCapability::TerminalFocus.granted_to(IngressSource::test_boot(), &config),
            "the focus grant is not implied by the lifecycle grant"
        );
        assert!(SceneCapability::AvatarScene.granted_to(IngressSource::test_boot(), &config));
    }

    #[test]
    fn avatar_scene_defaults_to_granted() {
        // A single-tenant session directs its own scene — the
        // `allow_scene_ambient` posture.
        let config = AppConfig::default();
        assert!(SceneCapability::AvatarScene.granted_to(IngressSource::test_boot(), &config));
    }

    #[test]
    fn trust_section_parses_and_revokes() {
        let config = AppConfig::from_toml_str("[trust.local]\navatar_scene = false\n")
            .expect("trust section parses");
        assert!(!SceneCapability::AvatarScene.granted_to(IngressSource::test_boot(), &config));
        // Unrelated grants are untouched by the [trust] section.
        assert!(SceneCapability::SceneAmbient.granted_to(IngressSource::test_boot(), &config));
    }

    #[test]
    fn absent_trust_section_defaults_cleanly() {
        let config = AppConfig::from_toml_str("").expect("empty config parses");
        assert!(SceneCapability::AvatarScene.granted_to(IngressSource::test_boot(), &config));
        assert!(
            !SceneCapability::TerminalLifecycle.granted_to(IngressSource::test_boot(), &config)
        );
        assert!(!SceneCapability::TerminalFocus.granted_to(IngressSource::test_boot(), &config));
    }
}
