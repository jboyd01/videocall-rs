import { test, expect, Page, Route } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * E2E test for the #1391 fix (follow-up #1394): bounded SSE re-establish on an
 * in-band `token_expired` frame.
 *
 * THE BEHAVIOUR UNDER TEST (chat_sidebar.rs + jmap_service.rs on this branch):
 * When the chat SSE stream delivers
 * `{"type":"auth","status":"token_expired"}`, the client drops the EventSource,
 * refreshes the OAuth token, re-POSTs `/sse/session`, and reopens — but with a
 * BOUNDED retry budget (`SSE_MAX_REESTABLISH_ATTEMPTS = 4`). When the budget is
 * exhausted it sets the terminal `connection_lost` state, which renders the
 * `.chat-error` banner "Chat disconnected (session expired). Reload the page to
 * reconnect." and STOPS. The bug this guards against is the pre-#1391 loop that
 * reconnected forever (observed 2393× in one call) — and the #1394 follow-up
 * concern that the re-establish path itself could become an unbounded refresh
 * storm. This test proves it TERMINATES.
 *
 * WHY THIS IS INTERCEPTABLE WITHOUT A CHAT BACKEND:
 * In the e2e stack `config.js` sets no `jmapBaseUrl`, so `jmap_base_url()`
 * returns "" and `jmap_origin_base()` (jmap_service.rs) falls back to the
 * page origin. That makes `/jmap`, `/sse/session`, and `/sse` SAME-ORIGIN
 * (http://localhost:3001/...) and therefore interceptable with `page.route()`
 * — no Docker chat service required.
 *
 * WHY THE LOOP REACHES GiveUp DETERMINISTICALLY (cookie / non-PKCE deployment):
 * The e2e stack sets `ENABLE_OAUTH=false` and no `OAUTH_FLOW`
 * (docker/docker-compose.e2e.yaml:269, docker/start-dioxus.sh:23,41), so
 * `is_pkce_flow()` is FALSE. After the #1469 fix the Retry arm gates the
 * provider refresh on `should_attempt_refresh(is_pkce_flow())`: on this
 * non-PKCE stack it SKIPS the (doomed) refresh and REOPENS directly on every
 * attempt — the reopen re-auths via the `sse_session` cookie / a fresh
 * `POST /sse/session`. Because the GET /sse stub below re-emits `token_expired`
 * on EVERY reopen, each reopen charges one attempt and the budget still climbs
 * to GiveUp after `SSE_MAX_REESTABLISH_ATTEMPTS` reopens. So `sseHits` now
 * climbs with the budget (initial mount + up to 4 reopens) instead of staying
 * ~1 — see the bound below.
 *
 * (Pre-#1469 the refresh ran unconditionally, returned `Err("no refresh token
 * stored")` instantly, and the loop charged at the refresh-fail site and
 * `continue`d WITHOUT reopening, so chat receive died permanently and `sseHits`
 * stayed ~1. The second test in this file is the regression proof for that fix.)
 *
 * Backoff schedule (base 750ms, doubling, +0–375ms jitter): 750 / 1500 / 3000 /
 * 6000ms ≈ 11–13s total before GiveUp.
 */

const TOKEN_EXPIRED_FRAME = 'data: {"type":"auth","status":"token_expired"}\n\n';

// A PROACTIVE WARNING frame: the credential is STILL VALID and the frame carries
// an `expires_in`. `is_sse_auth_failure` (jmap_service.rs) classifies this as a
// NON-failure, so it must NOT tear the stream down (the #1469 secondary fix).
const TOKEN_EXPIRING_FRAME = 'data: {"type":"auth","status":"token_expiring","expires_in":300}\n\n';

const DISCONNECT_BANNER = "Chat disconnected (session expired). Reload the page to reconnect.";

/**
 * Stub the JMAP method-call endpoint (`POST /jmap`).
 *
 * Every response MUST carry both `methodResponses` AND `sessionState` (camelCase)
 * or the client's `JmapResponse` deserialize fails (jmap_types.rs:11-20). We
 * inspect the POST body to decide which method is being called (see
 * jmap_service.rs get_or_create_conversation / get_messages_with_state):
 *
 *  - `Conversation/query` → empty `ids` drives the create path (step 1).
 *  - `Conversation/create` → returns a conversation id so `resolved_conv_id`
 *    resolves and the SSE effect + initial-load effect fire (step 3).
 *  - `ChatMessage/*` → empty list lets `is_loading` clear (so the banner branch,
 *    gated behind `!is_loading()`, can render) and shows no message bubbles.
 *  - anything else → a shape-valid empty envelope so a stray call never fails the
 *    deserialize.
 */
async function stubJmap(page: Page): Promise<void> {
  await page.route("**/jmap", async (route: Route) => {
    const body = route.request().postData() ?? "";
    if (body.includes("Conversation/query")) {
      await route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify({
          methodResponses: [
            ["Conversation/query", { accountId: "acc1", queryState: "0", ids: [] }, "0"],
          ],
          sessionState: "0",
        }),
      });
      return;
    }
    if (body.includes("Conversation/create")) {
      await route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify({
          methodResponses: [
            [
              "Conversation/create",
              { accountId: "acc1", list: [{ id: "conv-e2e-stub" }], state: "0" },
              "0",
            ],
          ],
          sessionState: "0",
        }),
      });
      return;
    }
    if (body.includes("ChatMessage/")) {
      await route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify({
          methodResponses: [
            ["ChatMessage/query", { accountId: "acc1", ids: [] }, "q"],
            ["ChatMessage/get", { accountId: "acc1", list: [], state: "0" }, "g"],
          ],
          sessionState: "0",
        }),
      });
      return;
    }
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ methodResponses: [], sessionState: "0" }),
    });
  });
}

/** Stub `POST /sse/session`: a 2xx lets `subscribe_chat_sse` proceed to open the
 *  EventSource (jmap_service.rs `subscribe_chat_sse`). Every reopen re-auths via
 *  a fresh POST here in cookie deployments. */
async function stubSseSession(page: Page): Promise<void> {
  await page.route("**/sse/session", async (route: Route) => {
    await route.fulfill({ status: 200 });
  });
}

/** Open the chat sidebar and wait for it to become visible. */
async function openChatSidebar(page: Page) {
  const sidebar = page.locator("#chat-sidebar");
  await page.getByRole("button", { name: "Open chat" }).click();
  await expect(sidebar).toHaveClass(/visible/, { timeout: 10_000 });
  return sidebar;
}

/** Navigate home, create+join a meeting, and wait for the call grid. */
async function joinMeeting(page: Page, meetingId: string, username: string): Promise<void> {
  await page.goto("/");
  await page.waitForTimeout(1500);

  await page.locator("#meeting-id").click();
  await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 80 });

  await page.locator("#username").click();
  await page.locator("#username").fill("");
  await page.locator("#username").pressSequentially(username, { delay: 80 });
  await page.waitForTimeout(500);
  await page.locator("#username").press("Enter");

  await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), {
    timeout: 10_000,
  });

  const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  await expect(joinButton).toBeVisible({ timeout: 20_000 });
  await joinButton.click();

  await expect(page.locator("#grid-container")).toBeVisible({ timeout: 15_000 });
}

test.describe("Chat SSE token-expiry bounded re-establish", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL }) => {
    const uniqueEmail = `e2e-chat-sse-${Date.now()}@videocall.rs`;
    await injectSessionCookie(context, { baseURL, email: uniqueEmail, name: "ChatUser" });
  });

  test("re-establish budget exhausts and shows a terminal disconnect banner (no infinite loop)", async ({
    page,
  }) => {
    const meetingId = `e2e_chat_sse_${Date.now()}`;

    // Counts every GET /sse hit. Each reopen during the re-establish loop hits
    // this route again; after GiveUp it must STOP increasing. This counter is
    // the termination proof.
    let sseHits = 0;

    await stubJmap(page);
    await stubSseSession(page);

    // ── Stub GET /sse (the EventSource stream) ───────────────────────────────
    // Every open returns a single `token_expired` frame, modelling a server that
    // re-emits the auth-failure on EVERY reopen. This drives the re-establish
    // loop and exercises the bounded-budget GiveUp path.
    await page.route("**/sse", async (route: Route) => {
      sseHits += 1;
      await route.fulfill({
        status: 200,
        contentType: "text/event-stream",
        body: TOKEN_EXPIRED_FRAME,
      });
    });

    // Routes registered above persist across the navigation joinMeeting() does.
    await joinMeeting(page, meetingId, "ChatUser");

    const sidebar = await openChatSidebar(page);

    // ── Assert the terminal disconnect banner appears ────────────────────────
    // `connection_lost` is set only on the GiveUp arm of the re-establish loop;
    // it renders a `.chat-error` div with this EXACT text (chat_sidebar.rs
    // `ChatSidebar`). The 20s timeout covers the ~11-13s backoff-to-GiveUp.
    const banner = sidebar.locator(".chat-error", { hasText: DISCONNECT_BANNER });
    await expect(banner).toBeVisible({ timeout: 20_000 });

    // ── TERMINATION PROOF ────────────────────────────────────────────────────
    // Once GiveUp fires the loop must STOP opening the EventSource. Snapshot the
    // /sse hit count, wait past one native EventSource reconnect window (~3s)
    // plus margin, and assert the count did not climb.
    //
    // What the bound counts: each `establish()` call opens the EventSource and
    // POSTs `/sse/session`, then GETs `/sse` (one hit). In THIS cookie / non-PKCE
    // environment the #1469 guard SKIPS the doomed refresh and REOPENS on every
    // Retry attempt (chat_sidebar.rs `should_attempt_refresh(is_pkce_flow())`
    // gate). Since the stub re-emits `token_expired` on every reopen, the budget
    // climbs to GiveUp after SSE_MAX_REESTABLISH_ATTEMPTS (4) reopens. So the
    // /sse hits are: initial-mount `establish(cid)` (1) + up to 4 re-establish
    // reopens = up to 5.
    //
    // The bound is SSE_MAX_REESTABLISH_ATTEMPTS (4) + initial mount (1) = 5 with a
    // small slack (≤ 6) to absorb jitter / an in-flight reopen at snapshot time.
    // The load-bearing assertion is the SECOND one: the count must NOT keep
    // growing after the terminal banner. If the cap regresses (e.g.
    // reset-on-bare-reopen reintroduced), the banner would never appear (the
    // first `expect` above already fails) AND `sseHits` would climb past this
    // bound here.
    const hitsAtGiveUp = sseHits;
    expect(hitsAtGiveUp).toBeLessThanOrEqual(6);

    await page.waitForTimeout(5000);

    // No further opens after the terminal state: the count is stable.
    expect(sseHits).toBeLessThanOrEqual(hitsAtGiveUp + 1);
    expect(sseHits).toBeLessThanOrEqual(6);
  });

  // ── #1469 REGRESSION: cookie-deployment recovery via reopen ────────────────
  //
  // THE BUG (#1469): on a cookie / non-PKCE deployment (exactly this e2e stack —
  // `ENABLE_OAUTH=false`, no `OAUTH_FLOW` ⇒ `is_pkce_flow()` is FALSE, and the
  // auth helper stores NO `vc_refresh_token`), the OLD Retry arm ran the provider
  // refresh unconditionally. With no refresh token it returned `Err` INSTANTLY
  // (no network), so the loop charged the attempt at the refresh-fail site and
  // `continue`d WITHOUT ever calling `subscribe_chat_sse` — the reopen. The
  // budget burned to GiveUp having NEVER reopened, so a stream that would have
  // re-authed fine via the `sse_session` cookie was permanently lost and the
  // disconnect banner appeared.
  //
  // THE FIX: gate the refresh on `should_attempt_refresh(is_pkce_flow())`. On a
  // cookie deployment the refresh is SKIPPED and the loop reopens directly.
  //
  // THIS TEST: the FIRST GET /sse emits `token_expired` then closes (forcing one
  // re-establish), but the SECOND+ GET /sse returns a HEALTHY stream (only a ping
  // heartbeat, never an auth-failure frame, stays open). Post-fix the non-PKCE
  // reopen path actually reopens, gets the healthy stream, and chat RECOVERS — no
  // disconnect banner. Pre-fix this scenario would NEVER reopen (it would burn the
  // budget on instant refresh errors) and would show the banner. The `sseHits >=
  // 2` assertion proves a real reopen happened — impossible on the old code here.
  test("cookie deployment recovers when a reopen yields a healthy stream (#1469)", async ({
    page,
  }) => {
    const meetingId = `e2e_chat_sse_recover_${Date.now()}`;

    let sseHits = 0;

    await stubJmap(page);
    await stubSseSession(page);

    // FIRST open → token_expired (forces exactly one re-establish). SUBSEQUENT
    // opens → a healthy stream: a single ping heartbeat, NO auth-failure frame,
    // and we leave it as a finished body (the client treats a clean close as a
    // benign EventSource reconnect, NOT an auth failure — only an in-band
    // `token_expired`/unknown-auth frame triggers teardown). The key invariant:
    // reopen #2+ never delivers an auth-failure frame, so `connection_lost` is
    // never set and the budget resets after probation.
    await page.route("**/sse", async (route: Route) => {
      sseHits += 1;
      if (sseHits === 1) {
        await route.fulfill({
          status: 200,
          contentType: "text/event-stream",
          body: TOKEN_EXPIRED_FRAME,
        });
        return;
      }
      // Healthy stream: heartbeat only, no auth frame.
      await route.fulfill({
        status: 200,
        contentType: "text/event-stream",
        body: "data: ping\n\n",
      });
    });

    await joinMeeting(page, meetingId, "ChatUser");

    const sidebar = await openChatSidebar(page);

    // The disconnect banner must NOT appear: the non-PKCE reopen path reopened,
    // got the healthy stream, and recovered. Wait past the first backoff + reopen
    // + probation window so a regression (no reopen ⇒ GiveUp banner) would have
    // surfaced. Pre-#1469 this banner WOULD appear (budget burned, never reopened).
    const banner = sidebar.locator(".chat-error", { hasText: DISCONNECT_BANNER });
    await expect(banner).toHaveCount(0);
    await page.waitForTimeout(8000);
    await expect(banner).toHaveCount(0);

    // A real reopen happened — impossible on the old non-PKCE code (which burned
    // the budget on instant refresh errors and never reached `subscribe_chat_sse`).
    expect(sseHits).toBeGreaterThanOrEqual(2);
  });

  // ── #1469 SECONDARY: a lone `token_expiring` WARNING must NOT tear down ─────
  //
  // THE SECONDARY FIX (jmap_service.rs `is_sse_auth_failure`): `token_expiring`
  // is a PROACTIVE warning — the credential is still valid and the frame carries
  // an `expires_in`. The old classifier matched any non-`ok`/`valid` auth status
  // as a failure, so a `token_expiring` frame tore the still-valid EventSource
  // down (cancelled ~2ms after the frame) and kicked off the re-establish loop
  // ~5 minutes early. The fix classifies `token_expiring` as a NON-failure.
  //
  // THIS TEST: every GET /sse emits a single `token_expiring` warning frame plus
  // a heartbeat, never an auth-FAILURE frame. Post-fix the warning is benign:
  // `is_sse_auth_failure` returns false, `on_auth_failure` is never invoked, the
  // client never enters the re-establish loop, and the disconnect banner NEVER
  // appears. The load-bearing assertion is `banner` absent across a generous
  // window. On the OLD classifier the warning was treated as a failure: it tore
  // the stream down, fired the re-establish loop, and — this being a non-PKCE
  // stack with no `vc_refresh_token` — the budget burned straight to the terminal
  // disconnect banner. So banner-absent cleanly distinguishes old vs new.
  //
  // NOTE on hit count: `route.fulfill` returns a FINISHED body, so the browser's
  // native EventSource reconnects (~3s) and re-serves another benign warning —
  // each is fine. We therefore do NOT assert an exact `sseHits`; the
  // discriminating signal is the absence of the banner (which a client-driven
  // teardown on this non-PKCE stack would inevitably produce).
  test("a lone token_expiring warning does not tear down a still-valid stream (#1469 secondary)", async ({
    page,
  }) => {
    const meetingId = `e2e_chat_sse_expiring_${Date.now()}`;

    let sseHits = 0;

    await stubJmap(page);
    await stubSseSession(page);

    // GET /sse: a `token_expiring` WARNING frame followed by a heartbeat — never
    // an auth-FAILURE frame. This is the state a still-valid stream is in when the
    // server proactively warns of an upcoming expiry.
    await page.route("**/sse", async (route: Route) => {
      sseHits += 1;
      await route.fulfill({
        status: 200,
        contentType: "text/event-stream",
        body: TOKEN_EXPIRING_FRAME + "data: ping\n\n",
      });
    });

    await joinMeeting(page, meetingId, "ChatUser");

    const sidebar = await openChatSidebar(page);

    // No teardown ⇒ no re-establish loop ⇒ no disconnect banner, ever. This is the
    // load-bearing assertion: on the OLD classifier the warning would be treated
    // as a failure and, on this non-PKCE stack, burn the budget to this banner.
    const banner = sidebar.locator(".chat-error", { hasText: DISCONNECT_BANNER });
    await expect(banner).toHaveCount(0);

    // Hold well past the full backoff-to-GiveUp window (~11-13s) so a wrongful
    // teardown would have surfaced the banner by now.
    await page.waitForTimeout(15000);

    await expect(banner).toHaveCount(0);
    // Sanity: the stream was opened at least once (the warning frame was actually
    // delivered). Native EventSource reconnects may push this above 1; that is
    // benign and not asserted against.
    expect(sseHits).toBeGreaterThanOrEqual(1);
  });
});
