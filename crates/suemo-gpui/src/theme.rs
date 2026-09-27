//! Shared look and feel: grid scales, theme colors (Q2 dark), interaction
//! constants. Hues are turns (0..1); the backdrop is translucent so the
//! desktop ghosts through around the column (grill round 5).

use gpui::{Hsla, hsla};

/// Day grid scale: pixels per minute (2880 px for 24 h, scrollable).
pub const PX_PER_MIN: f32 = 2.0;
pub const HOUR_PX: f32 = 60.0 * PX_PER_MIN;
pub const GRID_H: f32 = 24.0 * HOUR_PX;

/// Week grid scale: seven day columns at 1440 px per 24 h.
pub const WEEK_PX_PER_MIN: f32 = 1.0;
pub const WEEK_HOUR_PX: f32 = 60.0 * WEEK_PX_PER_MIN;
pub const WEEK_GRID_H: f32 = 24.0 * WEEK_HOUR_PX;

/// Hour-label gutters.
pub const LABEL_W: f32 = 56.0;
pub const WEEK_LABEL_W: f32 = 48.0;

pub const HEADER_H: f32 = 48.0;
/// Immersive centered column (round 5; other layouts come later).
pub const COLUMN_W: f32 = 880.0;

/// How close to a block edge a press must land to resize instead of move.
pub const EDGE_PX: f32 = 6.0;

/// Translucent backdrop behind the whole surface.
pub fn backdrop() -> Hsla {
    hsla(0.0, 0.0, 0.07, 0.92)
}

/// Editor panel and chips — nearly opaque.
pub fn panel() -> Hsla {
    hsla(0.0, 0.0, 0.10, 0.98)
}

/// Recessed input backgrounds.
pub fn field_bg() -> Hsla {
    hsla(0.0, 0.0, 1.0, 0.05)
}

/// Hairlines on fields/buttons.
pub fn field_border() -> Hsla {
    hsla(0.0, 0.0, 1.0, 0.12)
}

pub fn text_primary() -> Hsla {
    hsla(0.0, 0.0, 0.88, 1.0)
}

pub fn text_dim() -> Hsla {
    hsla(0.0, 0.0, 0.55, 1.0)
}

pub fn hour_line() -> Hsla {
    hsla(0.0, 0.0, 1.0, 0.06)
}

pub fn now_line() -> Hsla {
    hsla(0.02, 0.7, 0.55, 1.0)
}

/// Drag-preview fill and active-tab tint.
pub fn accent() -> Hsla {
    hsla(0.55, 0.5, 0.6, 1.0)
}

/// Error text.
pub fn danger() -> Hsla {
    hsla(0.99, 0.6, 0.62, 1.0)
}
