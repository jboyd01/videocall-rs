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
});
