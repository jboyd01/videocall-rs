/*
 * Copyright 2025 Security Union LLC
 *
 * Licensed under either of
 *
 * * Apache License, Version 2.0
 *   (http://www.apache.org/licenses/LICENSE-2.0)
 * * MIT license
 *   (http://opensource.org/licenses/MIT)
 *
 * at your option.
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

//! Issue 1175: zoom / pan / detach helpers for RECEIVED shared content (a peer's
//! screen share) on the receiving side.
//!
//! ## Why the canvas keeps painting while detached
//!
//! The screen-share decode pipeline (`videocall-client`'s
//! `peer_decode_manager` → `VideoPeerDecoder`) caches a DIRECT
//! `HtmlCanvasElement` reference and its `CanvasRenderingContext2d` inside the
//! decoder (`CanvasRenderer`) the moment the tile calls
//! `set_peer_screen_canvas`. Every decoded frame is drawn into that cached
//! context via `draw_image_*`; the paint path NEVER re-resolves the element
//! with `document.getElementById`. Because the reference is to the element
//! object itself, MOVING that same `<canvas>` node into another document (a
//! Document Picture-in-Picture window) does not invalidate the reference: the
//! decoder keeps painting the very same node live. That is the whole reason the
//! Document PiP detach approach works without a frozen image — see the detach
//! helpers in `screen_share_zoom_dom`.
//!
//! ## Pure logic lives here (host-testable)
//!
//! This module holds ONLY pure, DOM-free logic — zoom clamping, the
//! reset/fit value, and the next-step calculators — so it can be unit-tested on
//! the host (`cargo test`) without a browser. The imperative DOM glue (building
//! the viewport, wiring drag-to-pan, requesting / closing the PiP window) lives
//! in [`super::screen_share_zoom_dom`], which is `#[cfg(target_arch =
//! "wasm32")]`-only.
//!
//! `dead_code` is allowed at the module level: the only NON-test consumers of
//! the step/predicate helpers (`zoom_in`, `zoom_out`, `is_zoomed`, `ZOOM_STEP`)
//! live in the wasm-only `screen_share_zoom_dom` glue, so on a HOST build
//! (`cargo clippy --all`, no `--tests`) they have no caller and would otherwise
//! trip `-D warnings`. They ARE exercised — by the host `#[test]`s below and by
//! the wasm DOM module / integration tests — so the allow suppresses a
//! false-positive, not a real dead path.
#![allow(dead_code)]

/// Minimum zoom factor. 1.0 == fit-to-tile (the natural "100% of the tile"
/// state). We never shrink the content below the tile because the screen-share
/// canvas already letterboxes to the tile via CSS `object-fit: contain`, so a
/// sub-1.0 scale would just add empty margins with nothing to pan to.
pub const MIN_ZOOM: f64 = 1.0;

/// Maximum zoom factor. 4x is enough to read small UI text in a shared window
/// without letting the user lose the content entirely off-screen. Kept modest
/// because the zoom is a CSS width/height upscale of an already-rasterized
/// frame — beyond ~4x the upscaled pixels stop carrying useful detail.
pub const MAX_ZOOM: f64 = 4.0;

/// Multiplicative step applied per zoom-in / zoom-out click. 1.25 gives a
/// pleasant ~7 clicks across the full [1.0, 4.0] range.
pub const ZOOM_STEP: f64 = 1.25;

/// The reset / "actual fit" zoom level. Resetting returns the content to
/// exactly filling the tile (no pan offset, no scale).
pub const RESET_ZOOM: f64 = MIN_ZOOM;

/// Clamp an arbitrary zoom factor into the supported `[MIN_ZOOM, MAX_ZOOM]`
/// range. Used everywhere a zoom value is produced (button steps, future
/// wheel/pinch input) so an out-of-range value can never reach the DOM size.
///
/// `NaN` is treated as the reset level rather than propagating: a `NaN` CSS
/// size (`width: NaN%`) silently collapses the canvas, which would look like a
/// frozen/blank detach. Mapping it back to `RESET_ZOOM` keeps the content
/// visible.
pub fn clamp_zoom(z: f64) -> f64 {
    if z.is_nan() {
        return RESET_ZOOM;
    }
    z.clamp(MIN_ZOOM, MAX_ZOOM)
}

/// Next zoom level after a single zoom-IN click, clamped to range.
pub fn zoom_in(current: f64) -> f64 {
    clamp_zoom(clamp_zoom(current) * ZOOM_STEP)
}

/// Next zoom level after a single zoom-OUT click, clamped to range.
pub fn zoom_out(current: f64) -> f64 {
    clamp_zoom(clamp_zoom(current) / ZOOM_STEP)
}

/// Whether the content is currently zoomed in past the fit level. When `true`,
/// the viewport must be pannable (`overflow: auto` + drag-to-pan) and the
/// "reset" affordance is meaningful; when `false` there is nothing to pan, so
/// the viewport stays non-scrolling.
pub fn is_zoomed(z: f64) -> bool {
    clamp_zoom(z) > MIN_ZOOM
}

/// Format a zoom factor as an integer percentage label for the controls (e.g.
/// `1.0 -> "100%"`, `2.5 -> "250%"`). Rounds to the nearest percent so the
/// label is stable across the multiplicative steps.
pub fn zoom_percent_label(z: f64) -> String {
    let pct = (clamp_zoom(z) * 100.0).round() as i64;
    format!("{pct}%")
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- clamp_zoom ----------------------------------------------------------

    #[test]
    fn clamp_below_min_pins_to_min() {
        assert_eq!(clamp_zoom(0.2), MIN_ZOOM);
        assert_eq!(clamp_zoom(-5.0), MIN_ZOOM);
    }

    #[test]
    fn clamp_above_max_pins_to_max() {
        assert_eq!(clamp_zoom(10.0), MAX_ZOOM);
    }

    #[test]
    fn clamp_in_range_is_identity() {
        assert_eq!(clamp_zoom(1.0), 1.0);
        assert_eq!(clamp_zoom(2.5), 2.5);
        assert_eq!(clamp_zoom(4.0), 4.0);
    }

    #[test]
    fn clamp_nan_returns_reset() {
        // A NaN scale would blank the canvas (looks like a frozen detach);
        // it must map back to the visible reset level.
        assert_eq!(clamp_zoom(f64::NAN), RESET_ZOOM);
    }

    // --- zoom_in / zoom_out --------------------------------------------------

    #[test]
    fn zoom_in_steps_up_by_factor() {
        // From fit, one step in is exactly ZOOM_STEP.
        assert_eq!(zoom_in(1.0), ZOOM_STEP);
    }

    #[test]
    fn zoom_in_saturates_at_max() {
        // Already at max → stays at max, never overshoots.
        assert_eq!(zoom_in(MAX_ZOOM), MAX_ZOOM);
        // One step below max but past max*step → clamps to max.
        assert_eq!(zoom_in(3.5), MAX_ZOOM);
    }

    #[test]
    fn zoom_out_steps_down_by_factor() {
        // 2.0 / 1.25 = 1.6
        assert!((zoom_out(2.0) - 1.6).abs() < 1e-9);
    }

    #[test]
    fn zoom_out_saturates_at_min() {
        // At fit, zooming out cannot go below the tile.
        assert_eq!(zoom_out(MIN_ZOOM), MIN_ZOOM);
        assert_eq!(zoom_out(1.1), MIN_ZOOM);
    }

    #[test]
    fn zoom_in_then_out_is_stable_at_fit() {
        // A round trip from fit returns to fit (the out-clamp catches the
        // floating residue) so the reset button isn't the only way back.
        let up = zoom_in(1.0);
        assert_eq!(zoom_out(up), MIN_ZOOM);
    }

    // --- is_zoomed -----------------------------------------------------------

    #[test]
    fn is_zoomed_false_at_fit() {
        assert!(!is_zoomed(MIN_ZOOM));
        // Out-of-range low clamps to fit → not zoomed.
        assert!(!is_zoomed(0.3));
    }

    #[test]
    fn is_zoomed_true_when_scaled_up() {
        assert!(is_zoomed(1.5));
        assert!(is_zoomed(MAX_ZOOM));
    }

    // --- zoom_percent_label --------------------------------------------------

    #[test]
    fn percent_label_formats_round_values() {
        assert_eq!(zoom_percent_label(1.0), "100%");
        assert_eq!(zoom_percent_label(2.0), "200%");
        assert_eq!(zoom_percent_label(4.0), "400%");
    }

    #[test]
    fn percent_label_rounds_fractional() {
        // 1.25 -> 125%, 1.5625 -> 156%
        assert_eq!(zoom_percent_label(ZOOM_STEP), "125%");
        assert_eq!(zoom_percent_label(1.5625), "156%");
    }

    #[test]
    fn percent_label_clamps_out_of_range() {
        assert_eq!(zoom_percent_label(0.1), "100%");
        assert_eq!(zoom_percent_label(99.0), "400%");
    }
}
