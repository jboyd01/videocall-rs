import { test, expect, Page, BrowserContext, Browser, chromium } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * E2E: per-peer device / hardware metrics (issue #1482 — "show cpu, memory, and
 * hardware a peer uses if available").
 *
 * A peer self-reports its device facts in its periodic HealthPacket (the ~5 s
 * health interval): OS, device type, CPU cores, silicon architecture, device
 * memory, and main-thread load. The receiver parses those fields
 * (`video_call_client.rs` → `set_peer_device_info`) and surfaces them on TWO
 * render surfaces. Both surfaces render NOTHING when the peer reported nothing
 * (`Option::None` → omitted), so a real remote browser peer that actually emits
 * the metrics is required — a mock/video-off placeholder would never publish a
 * HealthPacket with device fields. This spec therefore drives a genuine 2-peer
 * call (host + camera-on guest) and waits through at least one health interval.
 *
 * The data plane is fully present in the e2e stack: the UI loads
 * `/console-log-collector.js`, which publishes `window.__videocall_client_metadata`
 * (OS / device_type / device_memory_gb / architecture) from `navigator.*` +
 * `userAgentData.getHighEntropyValues`; `navigator.hardwareConcurrency` feeds the
 * Cores field directly. All of OS / Device / Cores / Architecture / Memory are
 * reliably populated under the headless-Chromium runner used by Playwright. The
 * one field that may legitimately be absent is "Main-thread load", which is only
 * reported once a `'longtask'` PerformanceObserver entry has been seen — so it is
 * NOT asserted as required (its presence is treated as a bonus, never a gate).
 *
 * ## Surface 1 — Signal-quality popup ("Device" line)
 *
 *   div.signal-popup-device  [data-testid="signal-popup-device-{peer_id}"]
 *     span.signal-popup-device__head   "Device"
 *     span.signal-popup-device__line   "macOS 14.5 · desktop · 8 cores · arm · 8 GB · 42% load"
 *
 * The popup resolves device info per OPEN tile and is NOT gated on the receive
 * list — it renders the moment the peer's HealthPacket device fields have been
 * seen. (`signal_quality.rs::SignalQualityPopup`.)
 *
 * ## Surface 2 — Diagnostics drawer ("Device (per peer)" sub-block)
 *
 *   div.diag-device
 *     span.diag-device-title   "Device (per peer)"
 *     div.diag-device-peer     [data-testid="diag-device-peer-{session_id}"]
 *       span.diag-device-peer-label   {peer label}
 *       div.diag-device-row
 *         span.diag-device-row-label   {label, e.g. "Cores"}
 *         span.diag-device-row-value   {value, e.g. "8"}
 *
 * This sub-block lives under "Simulcast layers" → "Receiving (per peer)" in the
 * right-side Diagnostics drawer (`#diagnostics-sidebar`). It iterates the
 * per-peer RECEIVE list, so the peer must have media FLOWING (camera on) to
 * appear — which the 2-peer camera-on harness satisfies.
 * (`diagnostics.rs::SimulcastLayersSection` / `SimulcastReceiveBreakdown`.)
 *
 * ## Harness lineage
 *
 * The 2-peer camera-on join flow mirrors the proven helpers in
 * `signal-quality-peer-transport.spec.ts` (popup) and `simulcast-per-receiver.spec.ts`
 * (diagnostics drawer): home form → meeting URL → pre-join card → grant media +
 * camera-ON seed (`vc_prejoin_camera_on=true`) → race the Start/Join button vs.
 * the grid, admitting via the host's Waiting Room when the guest is parked.
 *
 * SERIAL + extended timeout: two heavy WebCodecs renderers (publisher encode +
 * receiver decode) plus a full health-interval wait. Matches the serial-mode
 * mitigation used by the simulcast spec for the 8-vCPU CI runner.
 */

const DEFAULT_UI_URL = "http://localhost:3001";

interface MeetingMember {
  page: Page;
  context: BrowserContext;
  email: string;
  name: string;
}

/**
 * Drive a context from the home form into the meeting URL, seeding the camera-ON
 * pre-join preference BEFORE navigation so the publisher actually emits video
 * (real browser peers default camera-OFF; without this seed no media flows and
 * the diagnostics receive list — hence its Device sub-block — stays empty). Does
 * NOT click Start/Join yet (that is handled by `clickJoinAndEnterGrid` so the
 * waiting-room admit flow can be interleaved).
 */
async function joinMeetingAs(
  context: BrowserContext,
  meetingId: string,
  username: string,
): Promise<Page> {
  const page = await context.newPage();
  await page.addInitScript(() => {
    try {
      window.localStorage.setItem("vc_prejoin_camera_on", "true");
    } catch {
      /* storage may be unavailable before origin navigation */
    }
  });

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

/**
 * Ensure the pre-join camera toggle is ON and a live preview track exists, so
 * the in-meeting encoder starts and the publisher actually sends video. The
 * persisted `vc_prejoin_camera_on=true` seed is the primary lever; this is the
 * belt-and-suspenders click + live-track wait mirroring the simulcast spec.
 */
async function ensurePrejoinCameraOn(page: Page): Promise<void> {
  const allow = page.locator('[data-testid="prejoin-permission-allow"]');
  if (await allow.isVisible().catch(() => false)) {
    await allow.click();
    await page
      .locator('[data-testid="prejoin-permission-prompt"]')
      .waitFor({ state: "hidden", timeout: 15_000 })
      .catch(() => {
        /* already granted / prompt absent */
      });
  }

  const cameraToggle = page.locator('[data-testid="prejoin-camera-toggle"]');
  if (!(await cameraToggle.isVisible().catch(() => false))) {
    return;
  }

  if ((await cameraToggle.getAttribute("aria-pressed")) !== "true") {
    await cameraToggle.click();
  }
  await expect(cameraToggle).toHaveAttribute("aria-pressed", "true", { timeout: 5_000 });

  await expect
    .poll(
      async () =>
        page
          .locator('[data-testid="prejoin-camera-preview"]')
          .evaluate((el) => {
            const v = el as HTMLVideoElement;
            const s = v.srcObject as MediaStream | null;
            return s ? s.getVideoTracks().filter((t) => t.readyState === "live").length : 0;
          })
          .catch(() => 0),
      { timeout: 15_000 },
    )
    .toBeGreaterThan(0);
}

/**
 * Race the pre-join Start/Join button against the grid (some joins auto-advance);
 * when the button appears, turn the camera on and click it. Mirrors
 * `signal-quality-peer-transport.spec.ts::clickJoinAndEnterGrid`.
 */
async function clickJoinAndEnterGrid(page: Page): Promise<void> {
  const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const grid = page.locator("#grid-container");

  const result = await Promise.race([
    joinButton.waitFor({ timeout: 30_000 }).then(() => "join" as const),
    grid.waitFor({ timeout: 30_000 }).then(() => "auto-joined" as const),
  ]);

  if (result === "join") {
    await ensurePrejoinCameraOn(page);
    await page.waitForTimeout(1000);
    await joinButton.click();
    await page.waitForTimeout(3000);
  }

  await expect(grid).toBeVisible({ timeout: 15_000 });
}

/**
 * Bring a host + one camera-on guest into the same meeting grid, handling the
 * waiting-room admit flow when the guest is parked. Returns the two members with
 * their live pages; the host sees exactly one remote peer tile.
 */
async function standUpTwoPeerCall(
  browsers: Browser[],
  uiURL: string,
  meetingId: string,
): Promise<MeetingMember[]> {
  const profiles = [
    { email: "host-dev@videocall.rs", name: "DevHost" },
    { email: "guest-dev@videocall.rs", name: "DevGuest" },
  ];

  const members: MeetingMember[] = [];
  for (let i = 0; i < 2; i++) {
    const ctx = await createAuthenticatedContext(
      browsers[i],
      profiles[i].email,
      profiles[i].name,
      uiURL,
    );
    members.push({
      page: null as unknown as Page,
      context: ctx,
      email: profiles[i].email,
      name: profiles[i].name,
    });
  }

  // Host joins first so the meeting is "active" before the guest arrives.
  members[0].page = await joinMeetingAs(members[0].context, meetingId, profiles[0].name);
  await clickJoinAndEnterGrid(members[0].page);

  // Guest joins. Handle direct-join / waiting-room / auto-join.
  members[1].page = await joinMeetingAs(members[1].context, meetingId, profiles[1].name);

  const joinButton = members[1].page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const waitingRoom = members[1].page.getByText("Waiting to be admitted");
  const guestGrid = members[1].page.locator("#grid-container");

  const result = await Promise.race([
    joinButton.waitFor({ timeout: 30_000 }).then(() => "join" as const),
    waitingRoom.waitFor({ timeout: 30_000 }).then(() => "waiting" as const),
    guestGrid.waitFor({ timeout: 30_000 }).then(() => "auto-joined" as const),
  ]);

  if (result === "waiting") {
    const admitButton = members[0].page.getByTitle("Admit").first();
    await expect(admitButton).toBeVisible({ timeout: 20_000 });
    await members[0].page.waitForTimeout(1000);
    await admitButton.dispatchEvent("click");
    await members[0].page.waitForTimeout(3000);
  }

  if (result !== "auto-joined") {
    await clickJoinAndEnterGrid(members[1].page);
  } else {
    await expect(guestGrid).toBeVisible({ timeout: 15_000 });
  }

  // Host should see exactly one remote peer tile.
  await expect(members[0].page.locator("#grid-container .canvas-container")).toHaveCount(1, {
    timeout: 30_000,
  });

  return members;
}

/**
 * Open the in-meeting Diagnostics drawer via the "Open Diagnostics" tooltip
 * button (it carries no data-testid). Mirrors
 * `simulcast-per-receiver.spec.ts::openPerformancePanel` /
 * `diagnostics-peer-transport.spec.ts`.
 */
async function openDiagnosticsDrawer(page: Page) {
  const diagButton = page.locator("button", {
    has: page.locator("span.tooltip", { hasText: "Open Diagnostics" }),
  });
  await diagButton.click();
  const drawer = page.locator("#diagnostics-sidebar");
  await expect(drawer).toBeVisible({ timeout: 10_000 });
  return drawer;
}

/**
 * The fixed device-row labels the panel emits, in render order
 * (`performance_settings.rs::format_peer_device_lines`). "Main-thread load" is
 * intentionally excluded from the REQUIRED set: it is reported only after a
 * `'longtask'` is observed, so on a quiet runner it may be legitimately absent.
 */
const ALWAYS_AVAILABLE_DEVICE_LABELS = ["OS", "Device", "Cores", "Architecture", "Memory"];

test.describe("Per-peer device / hardware metrics (#1482)", () => {
  // Heavy: two camera-on WebCodecs renderers + a full ~5 s health-interval wait
  // before device fields populate. Serial caps the peak heavy-renderer count on
  // the CI runner (mirrors simulcast-per-receiver.spec.ts).
  test.describe.configure({ mode: "serial", timeout: 180_000 });

  test.beforeAll(async () => {
    await waitForServices();
  });

  test("signal-quality popup shows the peer's compact Device line", async ({ baseURL }) => {
    const uiURL = baseURL || DEFAULT_UI_URL;
    const meetingId = `e2e_devmetrics_popup_${Date.now()}`;

    const browsers = await Promise.all([
      chromium.launch({ args: BROWSER_ARGS }),
      chromium.launch({ args: BROWSER_ARGS }),
    ]);
    const members: MeetingMember[] = [];

    try {
      members.push(...(await standUpTwoPeerCall(browsers, uiURL, meetingId)));
      const hostPage = members[0].page;

      // Open the signal-quality popup for the (single) remote peer tile.
      const signalButton = hostPage.locator(
        '#grid-container .canvas-container button[aria-label="Show signal quality"]',
      );
      await expect(signalButton).toBeVisible({ timeout: 15_000 });
      await signalButton.click();

      const popup = hostPage.locator(".signal-quality-popup");
      await expect(popup).toBeVisible({ timeout: 10_000 });

      // The Device block is omitted until the peer's HealthPacket device fields
      // have been seen (~5 s health interval). Poll through at least one interval.
      // testid is `signal-popup-device-{peer_id}`; with one remote peer there is
      // exactly one such element, so match on the stable prefix.
      const deviceBlock = popup.locator('[data-testid^="signal-popup-device-"]');
      await expect(deviceBlock).toBeVisible({ timeout: 45_000 });
      await expect(deviceBlock).toHaveClass(/\bsignal-popup-device\b/);

      // Head label is the literal "Device".
      await expect(deviceBlock.locator(".signal-popup-device__head")).toHaveText("Device");

      // The compact line is a non-empty dot-separated summary. It must contain at
      // least the always-available Cores token ("N cores") on a Chromium runner —
      // navigator.hardwareConcurrency is never absent — and use the " · "
      // separator the formatter joins with.
      const line = deviceBlock.locator(".signal-popup-device__line");
      await expect(line).toBeVisible();
      await expect(line).toHaveText(/\S/);
      await expect(line).toHaveText(/\d+ cores/);
      await expect(line).toContainText(" · ");
    } finally {
      for (const m of members) {
        if (m.page) {
          await m.page.close().catch(() => undefined);
        }
        await m.context.close().catch(() => undefined);
      }
      await Promise.all(browsers.map((b) => b.close().catch(() => undefined)));
    }
  });

  test("diagnostics drawer shows the 'Device (per peer)' sub-block", async ({ baseURL }) => {
    const uiURL = baseURL || DEFAULT_UI_URL;
    const meetingId = `e2e_devmetrics_diag_${Date.now()}`;

    const browsers = await Promise.all([
      chromium.launch({ args: BROWSER_ARGS }),
      chromium.launch({ args: BROWSER_ARGS }),
    ]);
    const members: MeetingMember[] = [];

    try {
      members.push(...(await standUpTwoPeerCall(browsers, uiURL, meetingId)));
      const hostPage = members[0].page;

      const drawer = await openDiagnosticsDrawer(hostPage);

      // The "Simulcast layers" section must mount (its per-peer RECEIVE list is
      // what the Device sub-block iterates).
      const simulcastSection = drawer.locator(".diagnostics-section", {
        has: hostPage.getByRole("heading", { name: "Simulcast layers" }),
      });
      await expect(simulcastSection).toBeVisible({ timeout: 30_000 });

      // The Device (per peer) block appears once the peer's HealthPacket device
      // fields have been parsed AND the peer is in the receive list (media
      // flowing). Poll through at least one ~5 s health interval.
      const deviceContainer = drawer.locator(".diag-device");
      await expect(deviceContainer).toBeVisible({ timeout: 45_000 });
      await expect(deviceContainer.locator(".diag-device-title")).toHaveText("Device (per peer)");

      // Exactly one remote peer → exactly one per-peer block, keyed by session_id.
      const peerBlock = deviceContainer.locator('[data-testid^="diag-device-peer-"]');
      await expect(peerBlock).toHaveCount(1, { timeout: 30_000 });
      await expect(peerBlock).toHaveClass(/\bdiag-device-peer\b/);

      // The peer label sub-element is present and non-empty (the guest's name).
      const peerLabel = peerBlock.locator(".diag-device-peer-label");
      await expect(peerLabel).toBeVisible();
      await expect(peerLabel).toHaveText(/\S/);

      // At least one label:value row is rendered, and each row exposes both the
      // label and the value span (the row contract).
      const rows = peerBlock.locator(".diag-device-row");
      await expect(rows.first()).toBeVisible({ timeout: 30_000 });
      const rowCount = await rows.count();
      expect(rowCount).toBeGreaterThan(0);

      // Every rendered row carries a non-empty label and a non-empty value, so we
      // never regress to an empty-labeled placeholder row.
      const labels: string[] = [];
      for (let i = 0; i < rowCount; i++) {
        const row = rows.nth(i);
        const label = (await row.locator(".diag-device-row-label").textContent())?.trim() ?? "";
        const value = (await row.locator(".diag-device-row-value").textContent())?.trim() ?? "";
        expect(label.length, `row ${i} label non-empty`).toBeGreaterThan(0);
        expect(value.length, `row ${i} value non-empty`).toBeGreaterThan(0);
        labels.push(label);
      }

      // The "Cores" row is always available on a Chromium runner
      // (navigator.hardwareConcurrency); assert it is one of the rendered labels
      // so the block proves a real device fact, not just structure. The other
      // always-available labels (OS / Device / Architecture / Memory) are a
      // superset we don't gate on individually to avoid runner-specific flake,
      // but every rendered label must be one of the known device-row labels.
      expect(labels).toContain("Cores");
      const knownLabels = [...ALWAYS_AVAILABLE_DEVICE_LABELS, "Main-thread load"];
      for (const label of labels) {
        expect(knownLabels, `unexpected device-row label "${label}"`).toContain(label);
      }
    } finally {
      for (const m of members) {
        if (m.page) {
          await m.page.close().catch(() => undefined);
        }
        await m.context.close().catch(() => undefined);
      }
      await Promise.all(browsers.map((b) => b.close().catch(() => undefined)));
    }
  });
});
