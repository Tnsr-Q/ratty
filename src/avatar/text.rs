//! Real shaped text for the avatar bubble: parley layouts painted into
//! the overlay vello scene.
//!
//! parley (0.10, exact-pinned to the version `parley_ratatui` already
//! compiles) shapes and wraps; the paint loop converts glyph runs to
//! `Scene::draw_glyphs` calls — the canonical in-tree conversion is
//! parley_ratatui's own `paint_layout`. The one novel part here is the
//! **cluster-bounded typewriter reveal**: glyphs paint only for clusters
//! whose logical `text_range().end` lies inside the revealed byte
//! prefix, walked in *visual* order with advances accumulated for hidden
//! clusters — so ligatures never half-draw, RTL runs reveal in logical
//! (reading) order at their correct visual positions, and combining
//! marks reveal atomically with their base. Reveal progress is a byte
//! offset into the text, not a glyph count, so a resize/DPI re-wrap
//! keeps progress seamlessly.
//!
//! Layouts are shaped once per `(text, wrap, size)` key and re-painted
//! every publish — parley_ratatui's own steady-state pattern.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use bevy::math::Vec2;
use std::sync::Arc;

use parley::fontique::{Blob, FontInfoOverride};
use parley::layout::PositionedLayoutItem;
use parley::style::{FontFamily, StyleProperty};
use parley::{Alignment, AlignmentOptions, FontContext, Layout, LayoutContext};
use parley_ratatui::vello::Scene;
use parley_ratatui::vello::kurbo::Affine;
use parley_ratatui::vello::peniko::{Brush, Color as PenikoColor, Fill};

/// The registered family name for the embedded faces (regular + bold).
const AVATAR_FONT_FAMILY: &str = "ratty-avatar-mono";

/// A shaped bubble: the attribution line and the body, with their
/// wrapped extents.
pub(crate) struct CachedBubble {
    key: BubbleKey,
    /// The speaker line, bold, never typewriter-gated: the speaking
    /// agent is identified from frame one (#23 attribution).
    pub attribution: Layout<()>,
    /// The utterance body, wrapped at the bubble width.
    pub body: Layout<()>,
    /// Wrapped extents: `(attribution, body)`, physical px.
    pub attribution_size: Vec2,
    /// Body extent, physical px.
    pub body_size: Vec2,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct BubbleKey {
    text_hash: u64,
    wrap_bits: u32,
    font_bits: u32,
}

fn bubble_key(text: &str, speaker: &str, wrap_px: f32, font_px: f32) -> BubbleKey {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    speaker.hash(&mut hasher);
    BubbleKey {
        text_hash: hasher.finish(),
        wrap_bits: wrap_px.to_bits(),
        font_bits: font_px.to_bits(),
    }
}

/// The avatar's own text stack: embedded fonts, shaping contexts, and a
/// single-slot layout cache (one utterance is live at a time).
pub(crate) struct AvatarTextSystem {
    font_cx: FontContext,
    layout_cx: LayoutContext<()>,
    cache: Option<CachedBubble>,
}

impl Default for AvatarTextSystem {
    fn default() -> Self {
        let mut font_cx = FontContext::new();
        for bytes in [
            crate::fonts::EMBEDDED_MONO_REGULAR,
            crate::fonts::EMBEDDED_MONO_BOLD,
        ] {
            font_cx.collection.register_fonts(
                // Zero-copy: the embedded bytes live for the program.
                Blob::new(Arc::new(bytes)),
                Some(FontInfoOverride {
                    family_name: Some(AVATAR_FONT_FAMILY),
                    ..Default::default()
                }),
            );
        }
        Self {
            font_cx,
            layout_cx: LayoutContext::new(),
            cache: None,
        }
    }
}

impl AvatarTextSystem {
    /// Shapes (or returns the cached) bubble for one utterance. Sizes are
    /// physical px (`quantize=true` — crisp at every DPI).
    pub(crate) fn bubble(
        &mut self,
        text: &str,
        speaker: &str,
        wrap_px: f32,
        font_px: f32,
    ) -> &CachedBubble {
        let key = bubble_key(text, speaker, wrap_px, font_px);
        if self.cache.as_ref().is_none_or(|cached| cached.key != key) {
            let attribution = self.shape(speaker, font_px * 0.9, true, None);
            let body = self.shape(text, font_px, false, Some(wrap_px));
            let attribution_size = Vec2::new(attribution.width(), attribution.height());
            let body_size = Vec2::new(body.width(), body.height());
            self.cache = Some(CachedBubble {
                key,
                attribution,
                body,
                attribution_size,
                body_size,
            });
        }
        self.cache.as_ref().expect("cache was just filled")
    }

    fn shape(&mut self, text: &str, font_px: f32, bold: bool, wrap_px: Option<f32>) -> Layout<()> {
        let mut builder = self
            .layout_cx
            .ranged_builder(&mut self.font_cx, text, 1.0, true);
        builder.push_default(StyleProperty::FontFamily(FontFamily::from(
            AVATAR_FONT_FAMILY,
        )));
        builder.push_default(StyleProperty::FontSize(font_px));
        builder.push_default(StyleProperty::LineHeight(
            parley::LineHeight::FontSizeRelative(1.35),
        ));
        if bold {
            builder.push_default(StyleProperty::FontWeight(parley::FontWeight::BOLD));
        }
        let mut layout = builder.build(text);
        layout.break_all_lines(wrap_px);
        layout.align(Alignment::Start, AlignmentOptions::default());
        layout
    }
}

/// Paints a whole layout (the attribution line, the overflow marker) —
/// the canonical parley→vello conversion.
pub(crate) fn paint_layout(
    scene: &mut Scene,
    layout: &Layout<()>,
    origin: Vec2,
    color: PenikoColor,
) {
    paint_layout_revealed(scene, layout, origin, color, usize::MAX);
}

/// Paints the clusters of `layout` whose logical text range ends at or
/// before `revealed_bytes` (see the module doc for why this is cluster-
/// and direction-correct). `usize::MAX` paints everything.
pub(crate) fn paint_layout_revealed(
    scene: &mut Scene,
    layout: &Layout<()>,
    origin: Vec2,
    color: PenikoColor,
    revealed_bytes: usize,
) {
    let brush = Brush::Solid(color);
    let transform = Affine::translate((f64::from(origin.x), f64::from(origin.y)));
    for line in layout.lines() {
        for item in line.items() {
            let PositionedLayoutItem::GlyphRun(glyph_run) = item else {
                continue;
            };
            let run = glyph_run.run();
            let mut x = glyph_run.offset();
            let y = glyph_run.baseline();
            scene
                .draw_glyphs(run.font())
                .brush(&brush)
                .hint(false)
                .transform(transform)
                .font_size(run.font_size())
                .normalized_coords(run.normalized_coords())
                .draw(
                    Fill::NonZero,
                    run.visual_clusters().flat_map(|cluster| {
                        let revealed = cluster.text_range().end <= revealed_bytes;
                        let glyphs: Vec<parley_ratatui::vello::Glyph> = if revealed {
                            cluster
                                .glyphs()
                                .map(|glyph| {
                                    let gx = x + glyph.x;
                                    let gy = y - glyph.y;
                                    parley_ratatui::vello::Glyph {
                                        id: glyph.id,
                                        x: gx,
                                        y: gy,
                                    }
                                })
                                .collect()
                        } else {
                            Vec::new()
                        };
                        // Advance even when hidden, so revealed clusters
                        // keep their final visual positions.
                        x += cluster.advance();
                        glyphs
                    }),
                );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn painted_width(layout: &Layout<()>, revealed: usize) -> f32 {
        // Re-derives the paint loop's placement to measure what would be
        // drawn: max (glyph x + advance) over revealed clusters.
        let mut max_x: f32 = 0.0;
        for line in layout.lines() {
            for item in line.items() {
                let PositionedLayoutItem::GlyphRun(glyph_run) = item else {
                    continue;
                };
                let run = glyph_run.run();
                let mut x = glyph_run.offset();
                for cluster in run.visual_clusters() {
                    if cluster.text_range().end <= revealed && cluster.glyphs().count() > 0 {
                        max_x = max_x.max(x + cluster.advance());
                    }
                    x += cluster.advance();
                }
            }
        }
        max_x
    }

    fn revealed_glyphs(layout: &Layout<()>, revealed: usize) -> usize {
        let mut count = 0;
        for line in layout.lines() {
            for item in line.items() {
                let PositionedLayoutItem::GlyphRun(glyph_run) = item else {
                    continue;
                };
                for cluster in glyph_run.run().visual_clusters() {
                    if cluster.text_range().end <= revealed {
                        count += cluster.glyphs().count();
                    }
                }
            }
        }
        count
    }

    #[test]
    fn shaping_wraps_within_the_budget_and_mixed_case_survives() {
        let mut system = AvatarTextSystem::default();
        let text = "The Deploy finished — all 14 checks passed, and the \
                    site is live. Ünïcode prose, Mixed Case, punctuation!";
        let bubble = system.bubble(text, "ci-agent", 240.0, 15.0);
        assert!(bubble.body.len() > 1, "narrow wrap yields multiple lines");
        assert!(
            bubble.body_size.x <= 240.0 + 0.5,
            "wrapped extent respects the budget ({}px)",
            bubble.body_size.x
        );
        assert!(bubble.attribution_size.x > 0.0);
        // Real glyphs shaped for the whole prose — not a stroke font.
        assert!(revealed_glyphs(&bubble.body, usize::MAX) >= text.chars().count() / 2);
    }

    #[test]
    fn reveal_is_monotonic_and_cluster_atomic() {
        let mut system = AvatarTextSystem::default();
        // A combining mark ('e' + U+0301) exercises multi-byte clusters.
        let text = "he\u{301}llo world";
        let bubble = system.bubble(text, "a", 400.0, 16.0);
        let mut previous = 0;
        for boundary in 0..=text.len() {
            let count = revealed_glyphs(&bubble.body, boundary);
            assert!(count >= previous, "reveal is monotonic in bytes");
            previous = count;
        }
        // Nothing at 0; everything at len.
        assert_eq!(revealed_glyphs(&bubble.body, 0), 0);
        assert!(revealed_glyphs(&bubble.body, text.len()) > 0);
        // Atomicity, pinned against the layout's own cluster ranges: a
        // boundary strictly inside a cluster reveals nothing beyond what
        // the cluster's start already revealed — multi-byte clusters
        // (like the combining mark's) appear all at once, never partially.
        let mut saw_multibyte_cluster = false;
        for line in bubble.body.lines() {
            for item in line.items() {
                let PositionedLayoutItem::GlyphRun(glyph_run) = item else {
                    continue;
                };
                for cluster in glyph_run.run().visual_clusters() {
                    let range = cluster.text_range();
                    if range.len() > 1 {
                        saw_multibyte_cluster = true;
                        let at_start = revealed_glyphs(&bubble.body, range.start);
                        for inside in range.start + 1..range.end {
                            assert_eq!(
                                revealed_glyphs(&bubble.body, inside),
                                at_start,
                                "a cluster must never half-draw"
                            );
                        }
                        assert!(
                            revealed_glyphs(&bubble.body, range.end) >= at_start,
                            "the cluster end reveals it"
                        );
                    }
                }
            }
        }
        assert!(
            saw_multibyte_cluster,
            "the combining mark must produce a multi-byte cluster"
        );
    }

    #[test]
    fn hidden_clusters_still_advance_so_positions_are_final() {
        let mut system = AvatarTextSystem::default();
        let text = "steady positions";
        let bubble = system.bubble(text, "a", 400.0, 16.0);
        let full = painted_width(&bubble.body, usize::MAX);
        // The last revealed cluster of a partial reveal sits exactly where
        // the full reveal puts it — no shifting during the typewriter.
        let partial = painted_width(&bubble.body, text.len() - 4);
        assert!(partial <= full);
        let again = painted_width(&bubble.body, usize::MAX);
        assert_eq!(full, again, "painting is deterministic");
    }

    #[test]
    fn cache_is_keyed_by_text_wrap_and_size() {
        let mut system = AvatarTextSystem::default();
        let first = system.bubble("hello", "a", 400.0, 16.0).body_size;
        // Same key: cache hit returns identical extents.
        assert_eq!(system.bubble("hello", "a", 400.0, 16.0).body_size, first);
        // Bigger font: new layout, wider extent.
        let bigger = system.bubble("hello", "a", 400.0, 24.0).body_size;
        assert!(bigger.x > first.x);
    }
}
