// SPDX-License-Identifier: MIT OR Apache-2.0

//! Issue 1175: icons for the received-shared-content zoom / pan / detach
//! controls. Stroke-only line icons matching the existing crop / pin icon
//! visual language (`stroke: currentColor`, 24x24 view box, round caps).

use dioxus::prelude::*;

/// Magnifying glass with a "+" — zoom in.
#[component]
pub fn ZoomInIcon() -> Element {
    rsx! {
        svg {
            class: "w-8",
            xmlns: "http://www.w3.org/2000/svg",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "2",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            circle { cx: "11", cy: "11", r: "7" }
            line { x1: "21", y1: "21", x2: "16.65", y2: "16.65" }
            line { x1: "11", y1: "8", x2: "11", y2: "14" }
            line { x1: "8", y1: "11", x2: "14", y2: "11" }
        }
    }
}

/// Magnifying glass with a "-" — zoom out.
#[component]
pub fn ZoomOutIcon() -> Element {
    rsx! {
        svg {
            class: "w-8",
            xmlns: "http://www.w3.org/2000/svg",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "2",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            circle { cx: "11", cy: "11", r: "7" }
            line { x1: "21", y1: "21", x2: "16.65", y2: "16.65" }
            line { x1: "8", y1: "11", x2: "14", y2: "11" }
        }
    }
}

/// Circular arrow — reset zoom to 100% / actual size.
#[component]
pub fn ZoomResetIcon() -> Element {
    rsx! {
        svg {
            class: "w-8",
            xmlns: "http://www.w3.org/2000/svg",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "2",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            polyline { points: "1 4 1 10 7 10" }
            path { d: "M3.51 15a9 9 0 1 0 2.13-9.36L1 10" }
        }
    }
}

/// Box with an outward arrow — detach (pop out) the shared content into its own
/// window.
#[component]
pub fn DetachIcon() -> Element {
    rsx! {
        svg {
            class: "w-8",
            xmlns: "http://www.w3.org/2000/svg",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "2",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            path { d: "M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6" }
            polyline { points: "15 3 21 3 21 9" }
            line { x1: "10", y1: "14", x2: "21", y2: "3" }
        }
    }
}
