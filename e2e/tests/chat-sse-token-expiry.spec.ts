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
 * returns "" and `jmap_origin_base()` (jmap_service.rs:23-32) falls back to the
 * page origin. That makes `/jmap`, `/sse/session`, and `/sse` SAME-ORIGIN
 * (http://localhost:3001/...) and therefore interceptable with `page.route()`
 * — no Docker chat service required.
 *
 * WHY THE LOOP REACHES GiveUp DETERMINISTICALLY:
 * The e2e auth helper injects ONLY a `session` cookie and never stores a
 * `vc_refresh_token`. Each Retry calls
 * `meeting_api::refresh_token_single_flight()` →
 * `auth::refresh_access_token()`, which returns `Err("no refresh token
 * stored")` immediately (auth.rs:247-248) with NO network call. So every cycle
 * charges one attempt at the refresh-fail site and the budget climbs to GiveUp.
 * Backoff schedule (base 750ms, doubling, +0–375ms jitter): 750 / 1500 / 3000 /
 * 6000ms ≈ 11–13s total before GiveUp.
 */

const TOKEN_EXPIRED_FRAME = 'data: {"type":"auth","status":"token_expired"}\n\n';

const DISCONNECT_BANNER = "Chat disconnected (session expired). Reload the page to reconnect.";

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

    // ── Stub the JMAP method-call endpoint ───────────────────────────────────
    // Every response MUST carry both `methodResponses` AND `sessionState`
    // (camelCase) or the client's `JmapResponse` deserialize fails
    // (jmap_types.rs:11-20). We inspect the POST body to decide which method is
    // being called (see jmap_service.rs get_or_create_conversation /
    // get_messages_with_state).
    await page.route("**/jmap", async (route: Route) => {
      const body = route.request().postData() ?? "";
      if (body.includes("Conversation/query")) {
        // Empty `ids` drives the create path (get_or_create_conversation step 1).
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
        // Returns a conversation id so `resolved_conv_id` resolves and the SSE
        // effect + initial-load effect fire (jmap_service.rs step 3).
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
        // Initial load (get_messages_with_state) sends a ChatMessage/query +
        // ChatMessage/get batch. An empty list lets `is_loading` clear (so the
        // banner branch — gated behind `!is_loading()` — can render) and shows
        // no message bubbles.
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
      // Any other JMAP method (e.g. Conversation/join): respond with a
      // shape-valid empty envelope so a stray call never fails the deserialize.
      await route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify({ methodResponses: [], sessionState: "0" }),
      });
    });

    // ── Stub POST /sse/session ────────────────────────────────────────────────
    // subscribe_chat_sse() POSTs here first; a 2xx lets it proceed to open the
    // EventSource (jmap_service.rs:664-683).
    await page.route("**/sse/session", async (route: Route) => {
      await route.fulfill({ status: 200 });
    });

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

    const sidebar = page.locator("#chat-sidebar");
    await page.getByRole("button", { name: "Open chat" }).click();
    await expect(sidebar).toHaveClass(/visible/, { timeout: 10_000 });

    // ── Assert the terminal disconnect banner appears ────────────────────────
    // `connection_lost` is set only on the GiveUp arm of the re-establish loop;
    // it renders a `.chat-error` div with this EXACT text (chat_sidebar.rs:1026-
    // 1030). The 20s timeout comfortably covers the ~11–13s backoff-to-GiveUp.
    const banner = sidebar.locator(".chat-error", { hasText: DISCONNECT_BANNER });
    await expect(banner).toBeVisible({ timeout: 20_000 });

    // ── TERMINATION PROOF ────────────────────────────────────────────────────
    // Once GiveUp fires the loop must STOP opening the EventSource. Snapshot the
    // /sse hit count, wait past one native EventSource reconnect window (~3s)
    // plus margin, and assert the count did not climb.
    //
    // What the bound counts: each `establish()` call opens the EventSource and
    // POSTs `/sse/session`, then GETs `/sse` (one hit). In THIS environment the
    // per-cycle token refresh ALWAYS fails (no `vc_refresh_token` stored), so the
    // re-establish loop charges its attempt at the refresh-fail site and
    // `continue`s WITHOUT reopening (chat_sidebar.rs:754-768) — the reopen site
    // (B) at line 780-791 is never reached. So the only `/sse` hit is the
    // initial-mount `establish(cid)` (line 880); `sseHits` should be ~1.
    //
    // The bound is set to SSE_MAX_REESTABLISH_ATTEMPTS (4) + 1 = 5 with a small
    // slack (≤ 6) so it ALSO holds on a stack/config where the refresh succeeds
    // and the loop does reach the reopen site (then it is bounded by the budget:
    // initial mount + at most 4 re-establish reopens). The load-bearing
    // assertion is the SECOND one: the count must NOT keep growing after the
    // terminal banner. If the cap regresses (e.g. reset-on-bare-reopen
    // reintroduced), the banner would never appear (the first `expect` above
    // already fails) AND, on a refresh-succeeds path, `sseHits` would climb past
    // this bound here.
    const hitsAtGiveUp = sseHits;
    expect(hitsAtGiveUp).toBeLessThanOrEqual(6);

    await page.waitForTimeout(5000);

    // No further opens after the terminal state: the count is stable.
    expect(sseHits).toBeLessThanOrEqual(hitsAtGiveUp + 1);
    expect(sseHits).toBeLessThanOrEqual(6);
  });
});
