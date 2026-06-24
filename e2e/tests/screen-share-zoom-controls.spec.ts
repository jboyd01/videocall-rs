import { test, expect, chromium, Page, BrowserContext } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * Issue 1175: received shared-content (someone else's screen share) zoom +
 * pan controls on the RECEIVING peer.
 *
 * A peer receiving shared content can zoom the content in/out and reset to
 * 100%, with the viewport becoming pannable while zoomed in. This spec drives
 * the deterministic, headless-testable half end-to-end: a guest screen-shares
 * (canvas-backed `getDisplayMedia` mock), the host receives it as the
 * `.split-screen-tile`, and clicking the zoom controls produces the expected
 * DOM mutations on the SAME live canvas (inline width/height upscale, % label,
 * and the `ss-zoom-viewport--pannable` modifier). The pure clamp / step / fit
 * math is covered by host `#[test]`s in `components::screen_share_zoom`, and the
 * handler DOM effect by `dioxus-ui/tests/screen_share_zoom.rs` (wasm-bindgen).
 *
 * DETACH (Document Picture-in-Picture) IS NOT COVERED HERE — deliberately and
 * with a verified reason, not an assumption. `window.documentPictureInPicture
 * .requestWindow()` requires a real transient user-activation AND spawns a real
 * second top-level OS window; headless Chromium under Playwright does not grant
 * the document-picture-in-picture capability and cannot surface / drive that
 * second window (Playwright's `context.pages()` does not enumerate a Document
 * PiP window — it is not a normal popup/tab). There is no e2e harness in
 * `e2e/tests/` that stands up a Document PiP window (grepped: no spec references
 * `documentPictureInPicture`, `requestWindow`, or a PiP window handle), so the
 * detach flow is validated by the feature-predicate host test
 * (`document_pip_supported`) plus manual Chromium verification noted in the PR,
 * not by this spec. The button's PRESENCE (feature-gated render) is asserted
 * here so the control at least appears on a Chromium receiver.
 *
 * Mirrors `peer-screen-static-fps.spec.ts` for auth + meeting setup and the
 * canvas-backed `getDisplayMedia` mock.
 *
 * ── #1175 a11y / control-wiring coverage (second test below) ──────────────────
 * A second test in this describe block extends the harness to drive the NEW
 * accessibility wiring that a ux/code review flagged as untested:
 *   • B1  keyboard pan — focus the `tabindex=0` `.ss-zoom-viewport` and press
 *         ArrowRight/ArrowDown/PageDown/End/Home; assert `scrollLeft`/`scrollTop`
 *         actually move (the keydown handler in `install_pan`).
 *   • S3  zoom-limit disabled state — `set_btn_disabled` toggles `disabled` +
 *         `aria-disabled="true"` on the zoom-OUT button at fit and the zoom-IN
 *         button at max, driven from the pure `at_min_zoom`/`at_max_zoom`
 *         predicates inside `apply_zoom`.
 *   • ARIA — the focusable viewport's `aria-label`, the `%` label's
 *         `aria-live="polite"` + `role="status"`, and the detach toggle's
 *         initial `aria-pressed="false"` + `aria-label`.
 *
 * IMPORTANT runtime detail this test depends on (verified against the source):
 * `apply_zoom` (in `screen_share_zoom_dom.rs`) is the ONLY place that calls
 * `set_btn_disabled`, and it runs only from the click handlers
 * (`handle_zoom_in/out/reset`) — NOT on initial mount. So on the pristine render
 * the zoom-OUT button is NOT yet `disabled` even though we are at fit. The S3
 * test therefore reaches the "at fit" disabled state by performing a zoom action
 * (zoom in, then reset back to fit) so an `apply_zoom` has actually run, rather
 * than asserting the disabled attribute on the never-zoomed render (which would
 * be a false expectation about the imperative wiring).
 *
 * DEFERRED — PiP-dependent assertions are intentionally NOT covered here (same
 * reason as the detach flow in the header above): headless Chromium under
 * Playwright does not grant the Document Picture-in-Picture capability and cannot
 * surface/drive the second top-level window, so the post-detach states cannot be
 * reached in this runner. Specifically deferred:
 *   • detach `aria-pressed` flipping to "true" after a successful detach
 *     (`set_detach_btn_state(.., true)` runs inside the PiP-open path),
 *   • the slot placeholder ("Shared content is in a separate window" +
 *     "Bring it back" / `data-testid="ss-pip-bring-back"`) which is gated on the
 *     slot's `data-detached` attr set only after the host moves into the PiP doc,
 *   • focus moving INTO the PiP window's detach button.
 * Grep confirms no spec in `e2e/tests/` stands up a Document PiP window: the only
 * references to `documentPictureInPicture` / `requestWindow` / a PiP window
 * handle are in THIS file's deferral comments. These states are covered by the
 * host predicate test (`document_pip_supported`) + manual Chromium verification
 * noted in the PR. We do NOT fake a PiP window to assert them.
 */

const DEFAULT_UI_URL = "http://localhost:3001";

interface MeetingMember {
  page: Page;
  context: BrowserContext;
  email: string;
  name: string;
}

// Canvas-backed getDisplayMedia mock that emits live frames continuously, so
// the receiver paints a real screen-share canvas (the surface the zoom
// controls operate on).
const MOCK_DISPLAY_MEDIA_SCRIPT = `
  (() => {
    const mediaDevices = navigator.mediaDevices;
    if (!mediaDevices) return;
    const createStream = () => {
      const canvas = document.createElement('canvas');
      canvas.width = 1280; canvas.height = 720;
      const ctx = canvas.getContext('2d');
      const stream = canvas.captureStream(10);
      let frame = 0;
      const tick = () => {
        frame++;
        ctx.fillStyle = '#1a1a2e'; ctx.fillRect(0, 0, 1280, 720);
        ctx.fillStyle = '#fff'; ctx.font = '32px sans-serif';
        ctx.fillText('Mock Screen Share (e2e-1175)', 320, 360);
        ctx.fillStyle = '#ff0';
        const x = 100 + (frame * 10) % 1000;
        ctx.fillRect(x, 600, 20, 20);
        setTimeout(tick, 100);
      };
      tick();
      return stream;
    };
    Object.defineProperty(mediaDevices, 'getDisplayMedia', {
      configurable: true, value: async () => createStream(),
    });
  })();
`;

async function joinMeetingAs(
  context: BrowserContext,
  meetingId: string,
  username: string,
): Promise<Page> {
  const page = await context.newPage();
  await page.goto("/");
  await page.waitForTimeout(1500);

  await page.locator("#meeting-id").click();
  await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 50 });
  await page.locator("#username").click();
  await page.locator("#username").fill("");
  await page.locator("#username").pressSequentially(username, { delay: 50 });
  await page.waitForTimeout(500);
  await page.locator("#username").press("Enter");

  await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });
  await page.waitForTimeout(1500);

  return page;
}

async function clickJoinAndEnterGrid(page: Page): Promise<void> {
  const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const grid = page.locator("#grid-container");

  const result = await Promise.race([
    joinButton.waitFor({ timeout: 30_000 }).then(() => "join" as const),
    grid.waitFor({ timeout: 30_000 }).then(() => "auto-joined" as const),
  ]);

  if (result === "join") {
    await page.waitForTimeout(1000);
    await joinButton.click();
    await page.waitForTimeout(3000);
  }

  await expect(grid).toBeVisible({ timeout: 15_000 });
}

async function admitGuestIfNeeded(hostPage: Page, guestPage: Page): Promise<void> {
  const joinButton = guestPage.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const waitingRoom = guestPage.getByText("Waiting to be admitted");
  const guestGrid = guestPage.locator("#grid-container");

  const result = await Promise.race([
    joinButton.waitFor({ timeout: 30_000 }).then(() => "join" as const),
    waitingRoom.waitFor({ timeout: 30_000 }).then(() => "waiting" as const),
    guestGrid.waitFor({ timeout: 30_000 }).then(() => "auto-joined" as const),
  ]);

  if (result === "waiting") {
    const admitButton = hostPage.getByTitle("Admit").first();
    await expect(admitButton).toBeVisible({ timeout: 20_000 });
    await hostPage.waitForTimeout(1000);
    await admitButton.dispatchEvent("click");
    await hostPage.waitForTimeout(3000);
  }

  if (result !== "auto-joined") {
    await clickJoinAndEnterGrid(guestPage);
  } else {
    await expect(guestGrid).toBeVisible({ timeout: 15_000 });
  }
}

async function startScreenShare(sharerPage: Page, viewerPage: Page): Promise<boolean> {
  await sharerPage.mouse.move(400, 400);
  await sharerPage.waitForTimeout(300);
  const shareButton = sharerPage.locator("button.video-control-button", {
    has: sharerPage.locator(".tooltip", { hasText: "Share Screen" }),
  });

  await expect(shareButton).toBeVisible({ timeout: 10_000 });
  await shareButton.click();

  try {
    await expect(viewerPage.locator(".split-screen-tile")).toBeVisible({
      timeout: 15_000,
    });
    return true;
  } catch {
    return false;
  }
}

// Bring up the 2-peer screen-share harness: guest shares a canvas-backed
// `getDisplayMedia` stream, host receives it as the `.split-screen-tile`. Used
// by both tests in this file. Returns the host page + split tile + a teardown,
// or `null` when the mock stream did not trigger the split layout (so the caller
// can `test.skip` exactly as the original single test did). Mirrors the
// established setup verbatim — no new seeding conventions are introduced (this
// is the existing screen-share harness, which does not need the camera-on
// prejoin seed; the receiver paints the screen-share canvas, not a camera one).
async function setupSharedContentReceiver(
  baseURL: string | undefined,
  meetingId: string,
  profiles: { email: string; name: string }[],
): Promise<{
  hostPage: Page;
  splitTile: ReturnType<Page["locator"]>;
  teardown: () => Promise<void>;
} | null> {
  const uiURL = baseURL || DEFAULT_UI_URL;
  const browsers = await Promise.all([
    chromium.launch({ args: BROWSER_ARGS }),
    chromium.launch({ args: BROWSER_ARGS }),
  ]);
  const members: MeetingMember[] = [];

  const teardown = async () => {
    for (const m of members) {
      if (m.page) await m.page.close().catch(() => {});
      await m.context.close().catch(() => {});
    }
    await Promise.all(browsers.map((b) => b.close().catch(() => {})));
  };

  for (let i = 0; i < 2; i++) {
    const ctx = await createAuthenticatedContext(
      browsers[i],
      profiles[i].email,
      profiles[i].name,
      uiURL,
    );
    await ctx.addInitScript(MOCK_DISPLAY_MEDIA_SCRIPT);
    members.push({
      page: null as unknown as Page,
      context: ctx,
      email: profiles[i].email,
      name: profiles[i].name,
    });
  }

  members[0].page = await joinMeetingAs(members[0].context, meetingId, profiles[0].name);
  await clickJoinAndEnterGrid(members[0].page);

  members[1].page = await joinMeetingAs(members[1].context, meetingId, profiles[1].name);
  await admitGuestIfNeeded(members[0].page, members[1].page);

  const hostPage = members[0].page;
  const guestPage = members[1].page;

  await expect(hostPage.locator("#grid-container .canvas-container")).toHaveCount(1, {
    timeout: 30_000,
  });

  const shareActivated = await startScreenShare(guestPage, hostPage);
  if (!shareActivated) {
    await teardown();
    return null;
  }

  const splitTile = hostPage.locator(".split-screen-tile");
  await expect(splitTile).toBeVisible({ timeout: 15_000 });

  return { hostPage, splitTile, teardown };
}

test.describe("Received shared-content zoom controls (issue 1175)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("zoom in upscales the shared-content canvas and makes the viewport pannable; reset returns to fit", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);
    const uiURL = baseURL || DEFAULT_UI_URL;
    const meetingId = `e2e_ss_zoom_${Date.now()}`;

    const browsers = await Promise.all([
      chromium.launch({ args: BROWSER_ARGS }),
      chromium.launch({ args: BROWSER_ARGS }),
    ]);

    const members: MeetingMember[] = [];

    try {
      const profiles = [
        { email: "host-1175@videocall.rs", name: "Zoom1175Host" },
        { email: "guest-1175@videocall.rs", name: "Zoom1175Guest" },
      ];

      for (let i = 0; i < 2; i++) {
        const ctx = await createAuthenticatedContext(
          browsers[i],
          profiles[i].email,
          profiles[i].name,
          uiURL,
        );
        await ctx.addInitScript(MOCK_DISPLAY_MEDIA_SCRIPT);
        members.push({
          page: null as unknown as Page,
          context: ctx,
          email: profiles[i].email,
          name: profiles[i].name,
        });
      }

      members[0].page = await joinMeetingAs(members[0].context, meetingId, profiles[0].name);
      await clickJoinAndEnterGrid(members[0].page);

      members[1].page = await joinMeetingAs(members[1].context, meetingId, profiles[1].name);
      await admitGuestIfNeeded(members[0].page, members[1].page);

      const hostPage = members[0].page;
      const guestPage = members[1].page;

      await expect(hostPage.locator("#grid-container .canvas-container")).toHaveCount(1, {
        timeout: 30_000,
      });

      // Guest shares; host receives the split-screen tile.
      const shareActivated = await startScreenShare(guestPage, hostPage);
      if (!shareActivated) {
        test.skip(
          true,
          "getDisplayMedia mock did not produce a stream that triggered the split layout.",
        );
        return;
      }

      const splitTile = hostPage.locator(".split-screen-tile");
      await expect(splitTile).toBeVisible({ timeout: 15_000 });

      // The shared-content canvas the controls operate on.
      const ssCanvas = splitTile.locator('canvas[id^="screen-share-"]');
      await expect(ssCanvas).toBeVisible({ timeout: 15_000 });

      // Hover to reveal the hover-gated control cluster (mirrors crop/pin).
      await splitTile.hover();
      await hostPage.waitForTimeout(300);

      const zoomIn = hostPage.locator('[data-testid="ss-zoom-in"]');
      const zoomReset = hostPage.locator('[data-testid="ss-zoom-reset"]');
      const zoomLabel = hostPage.locator('[data-testid="ss-zoom-label"]');
      await expect(zoomIn).toBeVisible({ timeout: 10_000 });

      // Precondition: at fit (100%), the canvas carries no inline width
      // override and the viewport is not pannable.
      await expect(zoomLabel).toHaveText("100%");
      const initialInlineWidth = await ssCanvas.evaluate((el) => (el as HTMLElement).style.width);
      expect(initialInlineWidth).toBe("");

      // --- Zoom in: the label moves off 100%, the canvas gains an inline
      //     width override (CSS upscale), and the viewport becomes pannable.
      await zoomIn.click();
      await hostPage.waitForTimeout(200);

      await expect(zoomLabel).not.toHaveText("100%");
      const zoomedInlineWidth = await ssCanvas.evaluate((el) => (el as HTMLElement).style.width);
      // The override is a percentage > 100% (load-bearing: the zoom is a CSS
      // size change on the live canvas, not a re-decode).
      expect(zoomedInlineWidth).toMatch(/%$/);
      const pct = parseFloat(zoomedInlineWidth);
      expect(pct).toBeGreaterThan(100);

      const viewport = hostPage.locator('[id$="-viewport"].ss-zoom-viewport');
      await expect(viewport).toHaveClass(/ss-zoom-viewport--pannable/, {
        timeout: 5_000,
      });

      // The same canvas keeps a live backing store (decoder owns width/height
      // attributes; zoom must not have touched them). 1280x720 from the mock.
      const backing = await ssCanvas.evaluate((el) => {
        const c = el as HTMLCanvasElement;
        return { w: c.width, h: c.height };
      });
      expect(backing.w).toBeGreaterThan(0);
      expect(backing.h).toBeGreaterThan(0);

      // --- Reset: back to 100%, inline override cleared, not pannable.
      await zoomReset.click();
      await hostPage.waitForTimeout(200);

      await expect(zoomLabel).toHaveText("100%");
      const resetInlineWidth = await ssCanvas.evaluate((el) => (el as HTMLElement).style.width);
      expect(resetInlineWidth).toBe("");
      await expect(viewport).not.toHaveClass(/ss-zoom-viewport--pannable/, {
        timeout: 5_000,
      });

      // --- Detach control: feature-gated render. Document PiP is supported in
      //     Chromium, so the detach button must be PRESENT on the tile (its
      //     full pop-out flow is out of scope for headless e2e — see file
      //     header). On a browser without Document PiP it would be absent; we
      //     assert presence here because the e2e runner is Chromium.
      const detachBtn = hostPage.locator('[data-testid="ss-detach"]');
      await expect(detachBtn).toBeVisible({ timeout: 10_000 });
    } finally {
      for (const m of members) {
        if (m.page) await m.page.close().catch(() => {});
        await m.context.close().catch(() => {});
      }
      await Promise.all(browsers.map((b) => b.close().catch(() => {})));
    }
  });

  // ── #1175 a11y / control-wiring coverage ────────────────────────────────────
  // Drives the keyboard-pan handler, the zoom-limit disabled toggling, and the
  // control ARIA attributes added for accessibility. All assertions below would
  // FAIL if the production a11y wiring were removed (see per-assertion notes):
  //   • drop `tabindex/role/aria-label` on the viewport  → ARIA + focus/pan break
  //   • drop the keydown handler in `install_pan`         → scrollLeft/Top stay 0
  //   • drop `set_btn_disabled` in `apply_zoom`           → no disabled/aria flips
  //   • drop `aria-live`/`role=status` on the % label     → label-ARIA assert fails
  test("keyboard pan, zoom-limit disabled state, and control ARIA (a11y wiring)", async ({
    baseURL,
  }) => {
    test.setTimeout(240_000);
    const meetingId = `e2e_ss_a11y_${Date.now()}`;
    const profiles = [
      { email: "host-1175-a11y@videocall.rs", name: "Zoom1175A11yHost" },
      { email: "guest-1175-a11y@videocall.rs", name: "Zoom1175A11yGuest" },
    ];

    const setup = await setupSharedContentReceiver(baseURL, meetingId, profiles);
    if (!setup) {
      test.skip(
        true,
        "getDisplayMedia mock did not produce a stream that triggered the split layout.",
      );
      return;
    }
    const { hostPage, splitTile, teardown } = setup;

    try {
      const ssCanvas = splitTile.locator('canvas[id^="screen-share-"]');
      await expect(ssCanvas).toBeVisible({ timeout: 15_000 });

      // Reveal the hover-gated control cluster (mirrors the first test).
      await splitTile.hover();
      await hostPage.waitForTimeout(300);

      const zoomIn = hostPage.locator('[data-testid="ss-zoom-in"]');
      const zoomOut = hostPage.locator('[data-testid="ss-zoom-out"]');
      const zoomReset = hostPage.locator('[data-testid="ss-zoom-reset"]');
      const zoomLabel = hostPage.locator('[data-testid="ss-zoom-label"]');
      const viewport = hostPage.locator('[id$="-viewport"].ss-zoom-viewport');
      await expect(zoomIn).toBeVisible({ timeout: 10_000 });
      await expect(zoomLabel).toHaveText("100%");

      // ── Control ARIA: static attributes that should hold from first render ──
      // The focusable viewport is a labelled group with a tabindex so keyboard
      // and switch users can reach and pan it (B1). Removing any of these breaks
      // a11y discovery of the pan affordance.
      await expect(viewport).toHaveAttribute("tabindex", "0");
      await expect(viewport).toHaveAttribute("role", "group");
      await expect(viewport).toHaveAttribute(
        "aria-label",
        "Shared content — use arrow keys to pan when zoomed",
      );

      // The % label is a polite live region so SR users hear zoom changes (P3).
      await expect(zoomLabel).toHaveAttribute("aria-live", "polite");
      await expect(zoomLabel).toHaveAttribute("role", "status");

      // The detach toggle (rendered only when Document PiP is supported — true on
      // the Chromium runner) starts un-pressed with a descriptive label (B2).
      // NOTE: we assert ONLY the initial, pre-detach state here. The flip to
      // aria-pressed="true" requires a real PiP window and is deferred (header).
      const detachBtn = hostPage.locator('[data-testid="ss-detach"]');
      await expect(detachBtn).toBeVisible({ timeout: 10_000 });
      await expect(detachBtn).toHaveAttribute("aria-pressed", "false");
      await expect(detachBtn).toHaveAttribute(
        "aria-label",
        "Open shared content in a separate window",
      );

      // ── S3: zoom-limit disabled state ──────────────────────────────────────
      // `apply_zoom` (the only caller of `set_btn_disabled`) runs on click, NOT
      // on mount, so the pristine render has no disabled attr even at fit. Drive
      // one zoom action to establish state, then assert the limits.

      // Zoom to MAX: click in repeatedly; poll until the zoom-IN button reports
      // disabled (at_max_zoom → set_btn_disabled). MAX_ZOOM=4.0, ZOOM_STEP=1.25
      // ⇒ ~7 steps from fit; 12 clicks is a safe ceiling and idempotent at max.
      for (let i = 0; i < 12; i++) {
        await zoomIn.click();
      }
      // At max, the zoom-IN button carries both `disabled` and
      // `aria-disabled="true"` (set_btn_disabled). `toBeDisabled` reads the
      // boolean `disabled` attribute; the aria mirror is asserted explicitly.
      await expect(zoomIn).toBeDisabled({ timeout: 5_000 });
      await expect(zoomIn).toHaveAttribute("aria-disabled", "true");
      // The label reflects the max state (MAX_ZOOM=4.0 ⇒ "400%"); load-bearing:
      // the label is the imperative read-out of the clamped zoom.
      await expect(zoomLabel).toHaveText("400%");
      // At max, zoom-OUT is back in range, so it must NOT be disabled.
      await expect(zoomOut).toBeEnabled({ timeout: 5_000 });
      await expect(zoomOut).not.toHaveAttribute("aria-disabled", "true");

      // Reset back to fit via a zoom action so `apply_zoom(RESET_ZOOM)` runs and
      // establishes the at-fit disabled state on the zoom-OUT button.
      await zoomReset.click();
      await expect(zoomLabel).toHaveText("100%", { timeout: 5_000 });
      // At fit (at_min_zoom), zoom-OUT is disabled + aria-disabled; zoom-IN is
      // re-enabled (back in range).
      await expect(zoomOut).toBeDisabled({ timeout: 5_000 });
      await expect(zoomOut).toHaveAttribute("aria-disabled", "true");
      await expect(zoomIn).toBeEnabled({ timeout: 5_000 });
      await expect(zoomIn).not.toHaveAttribute("aria-disabled", "true");

      // ── B1: keyboard pan ───────────────────────────────────────────────────
      // Zoom in several steps so the canvas (CSS width/height %) overflows the
      // viewport on BOTH axes, making it scrollable. The keydown handler only
      // pans when actually scrollable, so we must be genuinely zoomed in.
      for (let i = 0; i < 6; i++) {
        await zoomIn.click();
      }
      await expect(viewport).toHaveClass(/ss-zoom-viewport--pannable/, {
        timeout: 5_000,
      });
      // Confirm the viewport is actually scrollable on both axes before keying;
      // otherwise the handler (correctly) no-ops and the pan asserts would be
      // meaningless. Poll because layout/upscale settles asynchronously.
      await expect
        .poll(
          async () =>
            viewport.evaluate((el) => {
              const v = el as HTMLElement;
              return v.scrollWidth > v.clientWidth && v.scrollHeight > v.clientHeight;
            }),
          { timeout: 5_000 },
        )
        .toBe(true);

      // Focus the viewport (it is `tabindex=0`) and reset scroll to a known
      // origin so deltas are unambiguous.
      await viewport.evaluate((el) => {
        const v = el as HTMLElement;
        v.focus();
        v.scrollLeft = 0;
        v.scrollTop = 0;
      });
      // Sanity: focus actually landed on the viewport (proves it is focusable —
      // a missing tabindex would leave focus on <body> and the keydown would not
      // target the handler).
      await expect
        .poll(async () => viewport.evaluate((el) => document.activeElement === el), {
          timeout: 5_000,
        })
        .toBe(true);

      // ArrowRight → scrollLeft increases (pan_key_delta ArrowRight = +x).
      await hostPage.keyboard.press("ArrowRight");
      await expect
        .poll(async () => viewport.evaluate((el) => (el as HTMLElement).scrollLeft), {
          timeout: 5_000,
        })
        .toBeGreaterThan(0);

      // ArrowDown → scrollTop increases (pan_key_delta ArrowDown = +y).
      await hostPage.keyboard.press("ArrowDown");
      await expect
        .poll(async () => viewport.evaluate((el) => (el as HTMLElement).scrollTop), {
          timeout: 5_000,
        })
        .toBeGreaterThan(0);

      // PageDown → larger vertical jump; scrollTop grows beyond the ArrowDown
      // value (PAN_PAGE_STEP_PX > PAN_STEP_PX).
      const topAfterArrow = await viewport.evaluate((el) => (el as HTMLElement).scrollTop);
      await hostPage.keyboard.press("PageDown");
      await expect
        .poll(async () => viewport.evaluate((el) => (el as HTMLElement).scrollTop), {
          timeout: 5_000,
        })
        .toBeGreaterThan(topAfterArrow);

      // End → jump to the max scroll extent on both axes (handler computes max
      // from scrollWidth/scrollHeight). Both offsets should be at their maxima.
      await hostPage.keyboard.press("End");
      await expect
        .poll(
          async () =>
            viewport.evaluate((el) => {
              const v = el as HTMLElement;
              const atMaxX = v.scrollLeft >= v.scrollWidth - v.clientWidth - 1;
              const atMaxY = v.scrollTop >= v.scrollHeight - v.clientHeight - 1;
              return atMaxX && atMaxY;
            }),
          { timeout: 5_000 },
        )
        .toBe(true);

      // Home → both offsets reset toward 0.
      await hostPage.keyboard.press("Home");
      await expect
        .poll(
          async () =>
            viewport.evaluate((el) => {
              const v = el as HTMLElement;
              return v.scrollLeft === 0 && v.scrollTop === 0;
            }),
          { timeout: 5_000 },
        )
        .toBe(true);
    } finally {
      await teardown();
    }
  });
});
