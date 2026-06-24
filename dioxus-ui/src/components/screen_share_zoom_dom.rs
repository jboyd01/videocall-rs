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

//! Issue 1175: imperative DOM glue for the received-shared-content viewport.
//!
//! All the browser-touching parts of the zoom / pan / detach feature live here
//! so [`super::screen_share_zoom`] can stay pure and host-testable. Everything
//! in this module is `wasm32`-only.
//!
//! ## Design: imperative, NOT Dioxus-reactive
//!
//! Zoom level and detach state are deliberately NOT Dioxus signals. The
//! controls are rendered ONCE by Dioxus as static markup with stable handlers;
//! those handlers mutate the DOM directly (the canvas `width`/`height` CSS
//! size, the viewport's pannable class, and — for detach — moving the subtree
//! between the grid and a Document PiP window). This sidesteps the core hazard:
//! if Dioxus re-rendered the screen-share subtree (because a zoom/detach SIGNAL
//! changed), its virtual-DOM diff could tear down and recreate the `<canvas>`
//! node. A new
//! node would NOT be the element the screen decoder cached a reference to (see
//! the module docs on `screen_share_zoom`), so paint would freeze. By keeping
//! the state imperative, Dioxus never *re-creates* the `<canvas>` node — a
//! props-driven re-render of this subtree diffs by retained node identity and
//! reuses the unchanged element rather than tearing it down — so the cached
//! decoder canvas reference survives re-renders and the same `<canvas>` element
//! survives the move to the PiP window and back, which is exactly what keeps it
//! painting live.

use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use web_sys::{Element, HtmlElement};

use super::screen_share_zoom::{
    at_max_zoom, at_min_zoom, clamp_zoom, is_zoomed, pan_key_delta, zoom_in, zoom_out, RESET_ZOOM,
};

/// DOM id of the movable subtree wrapper for a peer's shared content. This is
/// the single node moved between the grid and the Document PiP window, so it
/// must wrap the viewport + canvas + zoom controls.
pub fn pip_host_id(peer_id: &str) -> String {
    format!("screen-share-{peer_id}-pip-host")
}

/// DOM id of the pannable viewport (the `overflow:auto` scroll box) for a peer.
pub fn viewport_id(peer_id: &str) -> String {
    format!("screen-share-{peer_id}-viewport")
}

/// DOM id of the percentage label inside the zoom controls for a peer.
pub fn zoom_label_id(peer_id: &str) -> String {
    format!("screen-share-{peer_id}-zoom-label")
}

/// DOM id of the grid slot the subtree is reattached into when the PiP window
/// closes. This anchor stays in the grid even while detached (rendered empty),
/// so reattach always has a home to return to.
pub fn grid_slot_id(peer_id: &str) -> String {
    format!("screen-share-{peer_id}-slot")
}

/// DOM id of the detach toggle button for a peer. Stable so the imperative
/// a11y-state updates (`aria-pressed`, label/title) can find it across BOTH
/// documents — the button travels into the PiP doc with its subtree (B2).
pub fn detach_btn_id(peer_id: &str) -> String {
    format!("screen-share-{peer_id}-detach-btn")
}

/// DOM id of the zoom-IN button for a peer. Stable so [`apply_zoom`] can toggle
/// its `disabled` state at the max-zoom limit across both documents (S3).
pub fn zoom_in_btn_id(peer_id: &str) -> String {
    format!("screen-share-{peer_id}-zoom-in")
}

/// DOM id of the zoom-OUT button for a peer (S3, see [`zoom_in_btn_id`]).
pub fn zoom_out_btn_id(peer_id: &str) -> String {
    format!("screen-share-{peer_id}-zoom-out")
}

/// DOM id of the visually-hidden `aria-live` announcement region for a peer.
/// This region lives in the MAIN document only (NOT inside the movable host),
/// so detach/reattach announcements are heard by the main-document user even
/// while the content is popped out (B2).
pub fn live_region_id(peer_id: &str) -> String {
    format!("screen-share-{peer_id}-live")
}

/// Announce `msg` to assistive tech via the peer's main-document `aria-live`
/// region. Resolved against the MAIN document only: the region is a sibling of
/// the movable host and never travels into the PiP window, so a main-document
/// screen-reader user hears detach/reattach state changes (B2).
pub fn announce(peer_id: &str, msg: &str) {
    if let Some(win) = web_sys::window() {
        if let Some(doc) = win.document() {
            if let Some(region) = doc.get_element_by_id(&live_region_id(peer_id)) {
                region.set_text_content(Some(msg));
            }
        }
    }
}

/// Feature-detect the Document Picture-in-Picture API. Returns `true` only when
/// `window.documentPictureInPicture` exists (Chromium 116+). Firefox and Safari
/// return `false`, so the caller hides/disables the detach control there rather
/// than crashing.
pub fn document_pip_supported() -> bool {
    let win = match web_sys::window() {
        Some(w) => w,
        None => return false,
    };
    match js_sys::Reflect::get(&win, &JsValue::from_str("documentPictureInPicture")) {
        Ok(v) => !v.is_undefined() && !v.is_null(),
        Err(_) => false,
    }
}

/// Read the current zoom factor stashed on the host element's `data-zoom`
/// attribute. Defaults to the reset level when absent/unparseable so the first
/// click starts from a known state.
fn current_zoom(host: &Element) -> f64 {
    host.get_attribute("data-zoom")
        .and_then(|s| s.parse::<f64>().ok())
        .map(clamp_zoom)
        .unwrap_or(RESET_ZOOM)
}

/// Apply a zoom factor to the canvas inside `host`: stash it on `data-zoom`,
/// scale the canvas via CSS width/height (CSS-only — never re-rasterizes the
/// backing store, so it stays cheap and the decoder keeps painting at native
/// cost), toggle the viewport's pan affordance, and update the % label. See the
/// long comment in the body for why width/height is used instead of
/// `transform: scale()`.
fn apply_zoom(peer_id: &str, host: &Element, zoom: f64) {
    let zoom = clamp_zoom(zoom);
    let _ = host.set_attribute("data-zoom", &zoom.to_string());

    let doc = match host.owner_document() {
        Some(d) => d,
        None => return,
    };

    // The canvas is the screen-share canvas inside this host subtree. Query
    // within the host (not the global document) so a detached subtree in the
    // PiP document is still found via its own owner document.
    //
    // We scale the canvas via CSS WIDTH/HEIGHT (percentage of the viewport),
    // NOT `transform: scale()`. A `transform` does not change the element's
    // layout box, so it creates NO scrollable overflow — the viewport's
    // `overflow:auto` would never get scrollbars and `scrollLeft`/`scrollTop`
    // (drag-to-pan) would have nothing to move. Width/height scaling grows the
    // layout box, so native scroll AND drag-to-pan both work. Critically this
    // only stretches the DISPLAYED bitmap: the canvas BACKING STORE
    // (`canvas.width`/`canvas.height` attributes) is owned by the decoder and
    // is untouched here, so there is NO re-raster and NO re-decode per frame —
    // the decoder keeps painting at native cost and the GPU upscales the
    // already-rendered frame.
    if let Ok(Some(canvas)) = host.query_selector("canvas") {
        if let Ok(canvas) = canvas.dyn_into::<HtmlElement>() {
            let pct = (zoom * 100.0).round();
            let style = canvas.style();
            // Clear any stray transform from earlier builds / experiments.
            let _ = style.remove_property("transform");
            if is_zoomed(zoom) {
                // Override the base `width/height: 100% !important` screen-share
                // rule with an !important inline value so the canvas grows.
                let _ = style.set_property_with_priority("width", &format!("{pct}%"), "important");
                let _ = style.set_property_with_priority("height", &format!("{pct}%"), "important");
                // At >100% the content fills/exceeds the box, so a `contain`
                // letterbox would shrink it back; switch to `fill` so the
                // zoomed dimensions are honored exactly.
                let _ = style.set_property_with_priority("object-fit", "fill", "important");
            } else {
                // Back to fit: drop the inline overrides so the base rule
                // (100% + contain letterbox) governs again.
                let _ = style.remove_property("width");
                let _ = style.remove_property("height");
                let _ = style.remove_property("object-fit");
            }
        }
    }

    // Toggle pan: only scroll when zoomed past fit.
    if let Some(vp) = doc.get_element_by_id(&viewport_id(peer_id)) {
        if let Ok(vp) = vp.dyn_into::<HtmlElement>() {
            if is_zoomed(zoom) {
                let _ = vp.class_list().add_1("ss-zoom-viewport--pannable");
            } else {
                let _ = vp.class_list().remove_1("ss-zoom-viewport--pannable");
                // Reset scroll position when returning to fit so the next
                // zoom-in starts from the top-left, not a stale offset.
                vp.set_scroll_top(0);
                vp.set_scroll_left(0);
            }
        }
    }

    // Update the % label.
    if let Some(label) = doc.get_element_by_id(&zoom_label_id(peer_id)) {
        label.set_text_content(Some(&super::screen_share_zoom::zoom_percent_label(zoom)));
    }

    // S3: disable the zoom-IN button at max and the zoom-OUT button at min so
    // they don't silently no-op. Resolved against the host's owner document
    // (`doc`) so they're found in the PiP document while detached. The disabled
    // state is driven from the PURE predicates (host-tested) — never recomputed
    // inline here.
    set_btn_disabled(&doc, &zoom_in_btn_id(peer_id), at_max_zoom(zoom));
    set_btn_disabled(&doc, &zoom_out_btn_id(peer_id), at_min_zoom(zoom));
}

/// Set or clear the `disabled` + `aria-disabled` state of a button by id within
/// `doc`. `disabled` is a boolean attribute: its mere PRESENCE disables the
/// control, so we must REMOVE it (not set it to "false") to re-enable — else a
/// button stays stuck disabled after zooming back into range (S3).
fn set_btn_disabled(doc: &web_sys::Document, btn_id: &str, disabled: bool) {
    if let Some(btn) = doc.get_element_by_id(btn_id) {
        if disabled {
            let _ = btn.set_attribute("disabled", "");
            let _ = btn.set_attribute("aria-disabled", "true");
        } else {
            let _ = btn.remove_attribute("disabled");
            let _ = btn.remove_attribute("aria-disabled");
        }
    }
}

/// Handle a zoom-in button click for `peer_id`.
pub fn handle_zoom_in(peer_id: &str) {
    if let Some(host) = host_element(peer_id) {
        let next = zoom_in(current_zoom(&host));
        apply_zoom(peer_id, &host, next);
    }
}

/// Handle a zoom-out button click for `peer_id`.
pub fn handle_zoom_out(peer_id: &str) {
    if let Some(host) = host_element(peer_id) {
        let next = zoom_out(current_zoom(&host));
        apply_zoom(peer_id, &host, next);
    }
}

/// Handle a reset ("actual size" / 100%) button click for `peer_id`.
pub fn handle_reset(peer_id: &str) {
    if let Some(host) = host_element(peer_id) {
        apply_zoom(peer_id, &host, RESET_ZOOM);
    }
}

/// Resolve the host element across BOTH documents: while detached the subtree
/// lives in the PiP window's document, so a plain main-document lookup would
/// miss it. We try the main document first, then the open PiP window's
/// document.
fn host_element(peer_id: &str) -> Option<Element> {
    let id = pip_host_id(peer_id);
    let win = web_sys::window()?;
    if let Some(doc) = win.document() {
        if let Some(el) = doc.get_element_by_id(&id) {
            return Some(el);
        }
    }
    // Try the PiP window's document.
    if let Some(pip_win) = pip_window() {
        if let Some(doc) = pip_win.document() {
            if let Some(el) = doc.get_element_by_id(&id) {
                return Some(el);
            }
        }
    }
    None
}

/// The currently-open Document PiP `Window`, or `None`. Reads
/// `window.documentPictureInPicture.window`, which the browser sets to the PiP
/// window while one is open and to `null` after it closes.
fn pip_window() -> Option<web_sys::Window> {
    let win = web_sys::window()?;
    let dpip = js_sys::Reflect::get(&win, &JsValue::from_str("documentPictureInPicture")).ok()?;
    if dpip.is_undefined() || dpip.is_null() {
        return None;
    }
    let pip = js_sys::Reflect::get(&dpip, &JsValue::from_str("window")).ok()?;
    if pip.is_undefined() || pip.is_null() {
        return None;
    }
    pip.dyn_into::<web_sys::Window>().ok()
}

/// Copy every stylesheet from the main document into the PiP document so the
/// moved subtree keeps the app's CSS (the PiP document starts blank). Mirrors
/// the standard Document PiP cookbook: deep-clone the `<link rel=stylesheet>`
/// and `<style>` elements into the PiP `<head>`. A deep clone of a `<link>`
/// re-fetches the external sheet in the PiP context; a deep clone of a `<style>`
/// carries its inline rule text. This avoids ever reading `cssRules` (which
/// throws for cross-origin sheets), so no per-sheet error handling is needed.
fn copy_styles_into_pip(main_doc: &web_sys::Document, pip_doc: &web_sys::Document) {
    let pip_head = match pip_doc.head() {
        Some(h) => h,
        None => return,
    };

    // Same-origin by spec: the Document Picture-in-Picture window is always
    // created same-origin with its opener (it has no navigable URL of its own),
    // so cloning our own `<link>`/`<style>` nodes into it is safe and never
    // crosses an origin boundary. Do NOT "harden" this into reading
    // `sheet.cssRules` — that would throw a SecurityError for any
    // cross-origin-served stylesheet and is unnecessary here (issue 1175).
    if let Ok(links) = main_doc.query_selector_all("link[rel=\"stylesheet\"], style") {
        for i in 0..links.length() {
            if let Some(node) = links.item(i) {
                if let Ok(cloned) = node.clone_node_with_deep(true) {
                    let _ = pip_head.append_child(&cloned);
                }
            }
        }
    }
}

/// Detach the shared-content subtree into a new Document PiP window. The same
/// `<canvas>` node is moved (not recreated), so the cached decoder reference
/// stays valid and the content keeps live-updating.
///
/// `on_closed` is invoked (once) when the PiP window closes for ANY reason
/// (user closes it, presenter stops sharing and we close it, meeting ends), so
/// the caller can move the subtree back and reset detach UI state. The subtree
/// is moved back to its grid slot here before `on_closed` runs.
pub fn detach_to_pip<F>(peer_id: &str, on_closed: F)
where
    F: Fn() + 'static,
{
    if !document_pip_supported() {
        return;
    }
    let win = match web_sys::window() {
        Some(w) => w,
        None => return,
    };
    let main_doc = match win.document() {
        Some(d) => d,
        None => return,
    };

    // Resolve the API object and request a window. Size hints come from the
    // host's current rendered size so the PiP opens roughly content-shaped.
    let dpip = match js_sys::Reflect::get(&win, &JsValue::from_str("documentPictureInPicture")) {
        Ok(v) if !v.is_undefined() && !v.is_null() => v,
        _ => return,
    };
    let request_fn = match js_sys::Reflect::get(&dpip, &JsValue::from_str("requestWindow")) {
        Ok(f) => match f.dyn_into::<js_sys::Function>() {
            Ok(f) => f,
            Err(_) => return,
        },
        Err(_) => return,
    };

    let host = match host_element(peer_id) {
        Some(h) => h,
        None => return,
    };

    // Build the {width,height} options from the host's rendered rect, clamped
    // to sane bounds so we never request a 0x0 or absurd window.
    let rect = host.get_bounding_client_rect();
    let w = (rect.width().round() as i64).clamp(320, 2560);
    let h = (rect.height().round() as i64).clamp(240, 1600);
    let opts = js_sys::Object::new();
    let _ = js_sys::Reflect::set(
        &opts,
        &JsValue::from_str("width"),
        &JsValue::from_f64(w as f64),
    );
    let _ = js_sys::Reflect::set(
        &opts,
        &JsValue::from_str("height"),
        &JsValue::from_f64(h as f64),
    );

    let promise = match request_fn.call1(&dpip, &opts) {
        Ok(p) => p,
        Err(_) => return,
    };
    let promise: js_sys::Promise = match promise.dyn_into() {
        Ok(p) => p,
        Err(_) => return,
    };

    let peer_id = peer_id.to_string();
    wasm_bindgen_futures::spawn_local(async move {
        let pip_win_val = match wasm_bindgen_futures::JsFuture::from(promise).await {
            Ok(v) => v,
            Err(e) => {
                log::warn!("documentPictureInPicture.requestWindow failed: {e:?}");
                return;
            }
        };
        let pip_win: web_sys::Window = match pip_win_val.dyn_into() {
            Ok(w) => w,
            Err(_) => return,
        };
        let pip_doc = match pip_win.document() {
            Some(d) => d,
            None => return,
        };

        copy_styles_into_pip(&main_doc, &pip_doc);
        // Tag the PiP body so our PiP-only CSS (full-bleed dark backdrop) applies.
        if let Some(body) = pip_doc.body() {
            let _ = body.class_list().add_1("ss-pip-body");
        }

        // Move the SAME subtree node into the PiP body. The canvas element
        // object is unchanged, so the decoder's cached reference keeps painting.
        let host = match host_element(&peer_id) {
            Some(h) => h,
            None => return,
        };
        if let Some(body) = pip_doc.body() {
            // adoptNode transfers ownership across documents before append so
            // the node's ownerDocument matches the PiP document (some engines
            // require this for event handling to keep working).
            if let Ok(adopted) = pip_doc.adopt_node(&host) {
                let _ = body.append_child(&adopted);
            } else {
                let _ = body.append_child(&host);
            }
        }
        // Re-apply the current zoom so the canvas size / pan affordance / label
        // are correct in the new document (those lookups now resolve in
        // pip_doc), and mark the host detached. Both touch the same host, so do
        // them in one lookup.
        if let Some(h) = host_element(&peer_id) {
            apply_zoom(&peer_id, &h, current_zoom(&h));
            let _ = h.set_attribute("data-detached", "true");

            // B2: update the detach toggle's a11y state. The button travelled
            // INTO the PiP doc with this subtree, so resolve it via the host's
            // owner document (the PiP document now), set it "pressed", and swap
            // the label/title to the reattach copy.
            set_detach_btn_state(&h, &peer_id, true);
            // Move focus into the PiP window so keyboard/SR focus follows the
            // content out of the grid. Focus the detach button (now in PiP).
            if let Some(owner) = h.owner_document() {
                if let Some(btn) = owner.get_element_by_id(&detach_btn_id(&peer_id)) {
                    if let Ok(btn) = btn.dyn_into::<HtmlElement>() {
                        let _ = btn.focus();
                    }
                }
            }
        }

        // S1: mark the grid SLOT (which stayed in the MAIN doc) detached so its
        // placeholder ("Shared content is in a separate window" + Bring it back)
        // shows in the now-empty hole. The moved host is gone from the slot, so
        // this attr lives on the slot, not the host.
        set_slot_detached(&peer_id, true);

        // B2: announce the state change in the MAIN document so a main-doc SR
        // user hears it (the live region never travels into the PiP).
        announce(&peer_id, "Shared content opened in a separate window");

        // Wire the close handler: when the PiP window goes away (user closes
        // it, OR we close it on presenter-stop/meeting-end), move the subtree
        // back to its grid slot and notify the caller. `pagehide` fires for the
        // PiP window in all close paths.
        let peer_for_close = peer_id.clone();
        let close_cb = Closure::<dyn FnMut()>::new(move || {
            reattach_from_pip(&peer_for_close);
            on_closed();
        });
        let _ =
            pip_win.add_event_listener_with_callback("pagehide", close_cb.as_ref().unchecked_ref());
        // Leak the closure: it must outlive this scope to fire on close. The PiP
        // window is short-lived and torn down on close, so this is a bounded,
        // one-per-detach allocation, not an unbounded leak.
        close_cb.forget();
    });
}

/// Move the shared-content subtree from the PiP document back to its grid slot.
/// Safe to call when not detached (no-op if the slot already owns the host or
/// the host can't be found).
pub fn reattach_from_pip(peer_id: &str) {
    let win = match web_sys::window() {
        Some(w) => w,
        None => return,
    };
    let main_doc = match win.document() {
        Some(d) => d,
        None => return,
    };
    let slot = match main_doc.get_element_by_id(&grid_slot_id(peer_id)) {
        Some(s) => s,
        None => return,
    };
    let host = match host_element(peer_id) {
        Some(h) => h,
        None => return,
    };
    // Already home? Make sure the detach UI is reset, then nothing else to do.
    if let Some(parent) = host.parent_element() {
        if parent.id() == grid_slot_id(peer_id) {
            let _ = host.remove_attribute("data-detached");
            set_detach_btn_state(&host, peer_id, false);
            set_slot_detached(peer_id, false);
            return;
        }
    }
    // Adopt back into the main document, then append into the grid slot.
    if let Ok(adopted) = main_doc.adopt_node(&host) {
        let _ = slot.append_child(&adopted);
    } else {
        let _ = slot.append_child(&host);
    }
    if let Some(h) = host_element(peer_id) {
        let _ = h.remove_attribute("data-detached");
        // Re-apply zoom so the label / viewport / canvas size resolve against
        // the main document again (they were last applied in the PiP document).
        apply_zoom(peer_id, &h, current_zoom(&h));

        // B2: reset the detach toggle's a11y state (now resolving in the main
        // doc), restore the "open" label/title, and RESTORE FOCUS to it so the
        // keyboard user lands back on the control that triggered the round trip.
        set_detach_btn_state(&h, peer_id, false);
        if let Some(btn) = main_doc.get_element_by_id(&detach_btn_id(peer_id)) {
            if let Ok(btn) = btn.dyn_into::<HtmlElement>() {
                let _ = btn.focus();
            }
        }
    }

    // S1: the host is back in the grid slot, so clear the slot's detached flag
    // (hides the placeholder again).
    set_slot_detached(peer_id, false);

    // B2: announce the return in the MAIN document.
    announce(peer_id, "Shared content returned to the grid");
}

/// B2 helper: set the detach toggle's `aria-pressed` + label/title for the
/// given state. Resolved via the host's OWNER document so it's found whether the
/// button currently lives in the main doc or the PiP doc (it travels with the
/// subtree). `pressed == true` means content is detached (label → reattach
/// copy); `false` restores the original "open in a separate window" copy.
fn set_detach_btn_state(host: &Element, peer_id: &str, pressed: bool) {
    let owner = match host.owner_document() {
        Some(d) => d,
        None => return,
    };
    if let Some(btn) = owner.get_element_by_id(&detach_btn_id(peer_id)) {
        let _ = btn.set_attribute("aria-pressed", if pressed { "true" } else { "false" });
        let (label, title) = if pressed {
            (
                "Return shared content to the grid",
                "Return shared content to the grid",
            )
        } else {
            (
                "Open shared content in a separate window",
                "Open shared content in a separate window",
            )
        };
        let _ = btn.set_attribute("aria-label", label);
        let _ = btn.set_attribute("title", title);
    }
}

/// S1 helper: set/clear `data-detached` on the grid SLOT in the MAIN document.
/// The slot stays in the grid while the host pops out, so its placeholder
/// visibility is driven from the slot, not the (moved-away) host.
fn set_slot_detached(peer_id: &str, detached: bool) {
    let doc = match web_sys::window().and_then(|w| w.document()) {
        Some(d) => d,
        None => return,
    };
    if let Some(slot) = doc.get_element_by_id(&grid_slot_id(peer_id)) {
        if detached {
            let _ = slot.set_attribute("data-detached", "true");
        } else {
            let _ = slot.remove_attribute("data-detached");
        }
    }
}

/// Close the open Document PiP window for this peer, if any. Triggers the
/// `pagehide` close handler (which reattaches + notifies). Called when the
/// presenter stops sharing or the meeting ends while detached, so a stale PiP
/// window never lingers showing frozen content.
pub fn close_pip_if_open() {
    if let Some(pip_win) = pip_window() {
        let _ = pip_win.close();
    }
}

/// Whether the open PiP window currently hosts THIS peer's shared content.
/// Only one Document PiP window can exist per page, so a tile must not close a
/// PiP that belongs to a DIFFERENT peer (multi-peer sharing). Checks the PiP
/// document for this peer's host id.
pub fn is_pip_open_for(peer_id: &str) -> bool {
    if let Some(pip_win) = pip_window() {
        if let Some(doc) = pip_win.document() {
            return doc.get_element_by_id(&pip_host_id(peer_id)).is_some();
        }
    }
    false
}

/// Install drag-to-pan on the viewport: pointer-down starts a drag, pointer-move
/// pans by adjusting `scrollLeft`/`scrollTop`, pointer-up ends it. Only pans
/// while zoomed (the viewport is non-scrolling at fit, so dragging is a no-op).
/// Returns the closures so the caller can keep them alive for the element's
/// lifetime; dropping them detaches the listeners.
pub struct PanHandlers {
    _down: Closure<dyn FnMut(web_sys::PointerEvent)>,
    _move: Closure<dyn FnMut(web_sys::PointerEvent)>,
    _up: Closure<dyn FnMut(web_sys::PointerEvent)>,
    // B1: keyboard-pan listener. Kept alive for the element's lifetime exactly
    // like the pointer closures above; dropping it detaches the keydown handler.
    _key: Closure<dyn FnMut(web_sys::KeyboardEvent)>,
}

/// Attach drag-to-pan listeners to the viewport element for `peer_id`. No-op if
/// the viewport is not yet in the DOM. The handlers are stored in the returned
/// [`PanHandlers`]; the caller must keep it alive (e.g. in a `use_hook` slot).
pub fn install_pan(peer_id: &str) -> Option<PanHandlers> {
    let win = web_sys::window()?;
    let doc = win.document()?;
    let vp_el = doc.get_element_by_id(&viewport_id(peer_id))?;
    let vp: HtmlElement = vp_el.dyn_into().ok()?;

    // Shared drag state: (active, last_client_x, last_client_y).
    let state = std::rc::Rc::new(std::cell::Cell::new((false, 0.0_f64, 0.0_f64)));

    let down_state = state.clone();
    let down_vp = vp.clone();
    let on_down =
        Closure::<dyn FnMut(web_sys::PointerEvent)>::new(move |e: web_sys::PointerEvent| {
            // Only start a pan when actually scrollable (zoomed in).
            if down_vp.scroll_width() <= down_vp.client_width()
                && down_vp.scroll_height() <= down_vp.client_height()
            {
                return;
            }
            down_state.set((true, e.client_x() as f64, e.client_y() as f64));
            let _ = down_vp.set_attribute("data-panning", "true");
        });

    let move_state = state.clone();
    let move_vp = vp.clone();
    let on_move =
        Closure::<dyn FnMut(web_sys::PointerEvent)>::new(move |e: web_sys::PointerEvent| {
            let (active, lx, ly) = move_state.get();
            if !active {
                return;
            }
            let cx = e.client_x() as f64;
            let cy = e.client_y() as f64;
            let dx = cx - lx;
            let dy = cy - ly;
            move_vp.set_scroll_left(move_vp.scroll_left() - dx as i32);
            move_vp.set_scroll_top(move_vp.scroll_top() - dy as i32);
            move_state.set((true, cx, cy));
        });

    let up_state = state.clone();
    let up_vp = vp.clone();
    let on_up =
        Closure::<dyn FnMut(web_sys::PointerEvent)>::new(move |_e: web_sys::PointerEvent| {
            let (_a, lx, ly) = up_state.get();
            up_state.set((false, lx, ly));
            let _ = up_vp.remove_attribute("data-panning");
        });

    // B1 (WCAG 2.1.1): keyboard panning. A keyboard / switch user can zoom but
    // not drag, so arrow / page / Home / End keys pan the viewport here. We ONLY
    // act (and `preventDefault`) when the viewport is actually scrollable
    // (zoomed past fit) — matching the SAME scrollability gate the pointerdown
    // handler uses (above). At fit we do nothing and do NOT swallow the keys, so
    // normal focus navigation is never trapped.
    let key_vp = vp.clone();
    let on_key =
        Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(move |e: web_sys::KeyboardEvent| {
            // Same pannability check as pointerdown: nothing to scroll → bail
            // without preventing default.
            let pannable = key_vp.scroll_width() > key_vp.client_width()
                || key_vp.scroll_height() > key_vp.client_height();
            if !pannable {
                return;
            }
            let key = e.key();
            match key.as_str() {
                // Home / End need the max scroll extent, which the pure delta
                // helper can't know; handle them here against scroll_width /
                // scroll_height.
                "Home" => {
                    e.prevent_default();
                    key_vp.set_scroll_left(0);
                    key_vp.set_scroll_top(0);
                }
                "End" => {
                    e.prevent_default();
                    let max_x = key_vp.scroll_width() - key_vp.client_width();
                    let max_y = key_vp.scroll_height() - key_vp.client_height();
                    key_vp.set_scroll_left(max_x.max(0));
                    key_vp.set_scroll_top(max_y.max(0));
                }
                other => {
                    // Arrow / Page keys: pure delta calc (host-tested). Other
                    // keys → None → leave the event untouched.
                    if let Some((dx, dy)) =
                        pan_key_delta(other, key_vp.client_width(), key_vp.client_height())
                    {
                        e.prevent_default();
                        // REUSE the same scroll mutation the pointermove path
                        // uses (set_scroll_left / set_scroll_top).
                        key_vp.set_scroll_left(key_vp.scroll_left() + dx);
                        key_vp.set_scroll_top(key_vp.scroll_top() + dy);
                    }
                }
            }
        });

    let _ = vp.add_event_listener_with_callback("pointerdown", on_down.as_ref().unchecked_ref());
    let _ = vp.add_event_listener_with_callback("pointermove", on_move.as_ref().unchecked_ref());
    let _ = vp.add_event_listener_with_callback("pointerup", on_up.as_ref().unchecked_ref());
    let _ = vp.add_event_listener_with_callback("pointerleave", on_up.as_ref().unchecked_ref());
    let _ = vp.add_event_listener_with_callback("keydown", on_key.as_ref().unchecked_ref());

    Some(PanHandlers {
        _down: on_down,
        _move: on_move,
        _up: on_up,
        _key: on_key,
    })
}
