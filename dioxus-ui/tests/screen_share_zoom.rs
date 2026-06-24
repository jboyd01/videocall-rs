// Copyright 2025 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// Issue 1175: browser tests for the received-shared-content zoom controls'
// DOM effect. Document Picture-in-Picture itself is not drivable in headless
// Chromium (it needs a user gesture and a real second window), so these tests
// cover the DETERMINISTIC, headless-testable half: the zoom-in / zoom-out /
// reset handlers' observable mutation of the canvas size, the viewport's
// pannable class, and the percentage label. The detach path is exercised in
// production behind a Chromium feature gate (see PR notes); the pure feature
// predicate and clamp math are covered by host `#[test]`s in
// `components::screen_share_zoom`.

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

use dioxus_ui::components::screen_share_zoom::{MAX_ZOOM, ZOOM_STEP};
use dioxus_ui::components::screen_share_zoom_dom::{
    handle_reset, handle_zoom_in, handle_zoom_out, pip_host_id, viewport_id, zoom_label_id,
};
use wasm_bindgen::JsCast;
use wasm_bindgen_test::*;
use web_sys::{Element, HtmlCanvasElement};

wasm_bindgen_test_configure!(run_in_browser);

/// A unique peer id per test so the shared single `document` instance across
/// `wasm_bindgen_test` cases never collides on element ids.
fn unique_peer(tag: &str) -> String {
    format!("zoomtest-{tag}-{}", js_sys::Math::random())
}

/// Build the minimal shared-content subtree the zoom helpers operate on:
/// `#screen-share-{peer}-pip-host > #screen-share-{peer}-viewport >
/// canvas#screen-share-{peer}` plus the `#screen-share-{peer}-zoom-label`. The
/// helpers resolve the host by id and query the canvas / viewport / label
/// within it, exactly as the real tile renders them.
fn build_subtree(peer: &str) -> (Element, HtmlCanvasElement) {
    let doc = web_sys::window().unwrap().document().unwrap();
    let body = doc.body().unwrap();

    let host = doc.create_element("div").unwrap();
    host.set_id(&pip_host_id(peer));
    host.set_attribute("data-zoom", "1").unwrap();

    let viewport = doc.create_element("div").unwrap();
    viewport.set_id(&viewport_id(peer));
    viewport
        .set_attribute("class", "ss-zoom-viewport canvas-container video-on")
        .unwrap();

    let canvas = doc
        .create_element("canvas")
        .unwrap()
        .dyn_into::<HtmlCanvasElement>()
        .unwrap();
    canvas.set_id(&format!("screen-share-{peer}"));
    // The decoder owns the backing store; seed a representative size so the
    // tests can assert the helpers never touch the backing store attributes.
    canvas.set_width(1280);
    canvas.set_height(720);

    let label = doc.create_element("span").unwrap();
    label.set_id(&zoom_label_id(peer));
    label.set_text_content(Some("100%"));

    viewport.append_child(&canvas).unwrap();
    host.append_child(&viewport).unwrap();
    host.append_child(&label).unwrap();
    body.append_child(&host).unwrap();

    (host, canvas)
}

fn label_text(peer: &str) -> String {
    web_sys::window()
        .unwrap()
        .document()
        .unwrap()
        .get_element_by_id(&zoom_label_id(peer))
        .unwrap()
        .text_content()
        .unwrap_or_default()
}

fn viewport_class(peer: &str) -> String {
    web_sys::window()
        .unwrap()
        .document()
        .unwrap()
        .get_element_by_id(&viewport_id(peer))
        .unwrap()
        .get_attribute("class")
        .unwrap_or_default()
}

#[wasm_bindgen_test]
fn zoom_in_scales_canvas_and_updates_label() {
    let peer = unique_peer("in");
    let (host, canvas) = build_subtree(&peer);

    // Precondition: fit state, no inline width override, label 100%.
    assert_eq!(label_text(&peer), "100%");
    assert!(
        canvas
            .style()
            .get_property_value("width")
            .unwrap()
            .is_empty(),
        "canvas must start with no inline width override"
    );

    handle_zoom_in(&peer);

    // The label reflects exactly one ZOOM_STEP.
    let expected_pct = (ZOOM_STEP * 100.0).round() as i64;
    assert_eq!(label_text(&peer), format!("{expected_pct}%"));

    // The canvas DISPLAY size grows via an inline width/height override (the
    // load-bearing behavior: zoom is CSS-only, no re-raster). This is the
    // mutation that would break if `apply_zoom` stopped scaling the canvas.
    let inline_w = canvas.style().get_property_value("width").unwrap();
    assert_eq!(inline_w, format!("{expected_pct}%"));

    // The data-zoom attribute tracks the imperative zoom state.
    let z = host.get_attribute("data-zoom").unwrap();
    assert!((z.parse::<f64>().unwrap() - ZOOM_STEP).abs() < 1e-6);

    // The viewport becomes pannable when zoomed past fit.
    assert!(
        viewport_class(&peer).contains("ss-zoom-viewport--pannable"),
        "viewport must gain the pannable modifier when zoomed in"
    );

    // CRITICAL: the canvas BACKING STORE is untouched — zoom must never
    // re-rasterize (would defeat the per-frame-cheap requirement and could
    // race the decoder). If `apply_zoom` ever called set_width/set_height this
    // assertion fails.
    assert_eq!(canvas.width(), 1280);
    assert_eq!(canvas.height(), 720);
}

#[wasm_bindgen_test]
fn reset_returns_to_fit_and_clears_overrides() {
    let peer = unique_peer("reset");
    let (_host, canvas) = build_subtree(&peer);

    // Zoom in twice, then reset.
    handle_zoom_in(&peer);
    handle_zoom_in(&peer);
    assert!(viewport_class(&peer).contains("ss-zoom-viewport--pannable"));

    handle_reset(&peer);

    // Back to 100% and the inline overrides are cleared so the base
    // letterbox rule governs again.
    assert_eq!(label_text(&peer), "100%");
    assert!(
        canvas
            .style()
            .get_property_value("width")
            .unwrap()
            .is_empty(),
        "reset must clear the inline width override"
    );
    assert!(
        canvas
            .style()
            .get_property_value("height")
            .unwrap()
            .is_empty(),
        "reset must clear the inline height override"
    );
    assert!(
        !viewport_class(&peer).contains("ss-zoom-viewport--pannable"),
        "reset must remove the pannable modifier"
    );
}

#[wasm_bindgen_test]
fn zoom_out_at_fit_is_clamped_and_not_pannable() {
    let peer = unique_peer("out");
    let (host, _canvas) = build_subtree(&peer);

    // From fit, zooming out cannot go below the tile.
    handle_zoom_out(&peer);

    assert_eq!(label_text(&peer), "100%");
    let z = host.get_attribute("data-zoom").unwrap();
    assert!((z.parse::<f64>().unwrap() - 1.0).abs() < 1e-6);
    assert!(
        !viewport_class(&peer).contains("ss-zoom-viewport--pannable"),
        "at fit the viewport must not be pannable"
    );
}

#[wasm_bindgen_test]
fn zoom_in_saturates_at_max() {
    let peer = unique_peer("max");
    let (host, _canvas) = build_subtree(&peer);

    // Click far more than enough to exceed MAX_ZOOM; it must clamp, not run away.
    for _ in 0..20 {
        handle_zoom_in(&peer);
    }
    let z: f64 = host.get_attribute("data-zoom").unwrap().parse().unwrap();
    assert!(
        (z - MAX_ZOOM).abs() < 1e-6,
        "zoom must saturate at MAX_ZOOM"
    );
    let expected_pct = (MAX_ZOOM * 100.0).round() as i64;
    assert_eq!(label_text(&peer), format!("{expected_pct}%"));
}
