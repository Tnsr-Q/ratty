//! Embedded DejaVu Sans Mono, shared by the wasm terminal fallback and
//! the avatar bubble substrate: one copy of the bytes per face, embedded
//! on **both** targets so the bubble shapes the identical face natively
//! and on the site (system font discovery can miss the family; wasm has
//! no system fonts at all). License: `assets/fonts/LICENSE-DejaVu.txt`.

/// DejaVu Sans Mono, regular.
pub(crate) const EMBEDDED_MONO_REGULAR: &[u8] =
    include_bytes!("../assets/fonts/DejaVuSansMono.ttf");
/// DejaVu Sans Mono, bold (the attribution chip weight).
pub(crate) const EMBEDDED_MONO_BOLD: &[u8] =
    include_bytes!("../assets/fonts/DejaVuSansMono-Bold.ttf");
