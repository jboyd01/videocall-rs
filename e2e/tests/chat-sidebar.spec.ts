import { test, expect, Page } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * E2E test for the in-call chat sidebar (`ChatSidebar` in
 * dioxus-ui/src/components/chat_sidebar.rs).
 *
 * Covered flow: open chat -> type a message -> verify it appears ->
 * close sidebar -> reopen -> verify the message is still present.
 *
 * WHAT THIS TEST EXERCISES (and why it is reliable without a chat backend):
 * The chat send handler (`do_send`) optimistically pushes the typed message
 * into the component's local `messages` signal *before* any network call, so
 * the message echoes into the UI immediately. The `ChatSidebar` component is
 * always mounted in the meeting view (`attendants.rs`); the open/close toggle
 * only flips the `visible` CSS class, so the `messages` signal — including the
 * locally-echoed sent message — persists across close/reopen. Both of those
 * behaviors are pure client-side state and require no JMAP/Smatter backend.
 *
 * TODO (backend-dependent assertions): The e2e stack
 * (docker/docker-compose.e2e.yaml) does NOT include the JMAP/Smatter chat
 * backend. As a result this test cannot assert:
 *   - server-side persistence of sent messages,
 *   - delivery of messages between two participants over SSE,
 *   - the unread badge (`chat-unread-badge`) driven by `on_new_message`,
 *   - the "is_self vs other" rendering of messages fetched from the server.
 * When a chat backend is wired into the e2e stack, add a second test that:
 *   1. joins two participants in the same meeting,
 *   2. sends a message from participant A,
 *   3. asserts it arrives in participant B's sidebar via SSE,
 *   4. asserts the unread badge appears for B while the sidebar is closed.
 */

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

test.describe("In-call chat sidebar", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL }) => {
    const uniqueEmail = `e2e-chat-${Date.now()}@videocall.rs`;
    await injectSessionCookie(context, { baseURL, email: uniqueEmail, name: "ChatUser" });
  });

  test("typed message echoes into the sidebar and survives close/reopen", async ({ page }) => {
    const meetingId = `e2e_chat_${Date.now()}`;
    const messageText = `hello-from-e2e-${Date.now()}`;

    await joinMeeting(page, meetingId, "ChatUser");

    const sidebar = page.locator("#chat-sidebar");

    // ── Open the chat sidebar ────────────────────────────────────────────
    await page.getByRole("button", { name: "Open chat" }).click();
    await expect(sidebar).toHaveClass(/visible/, { timeout: 10_000 });
    await expect(sidebar.locator(".chat-input")).toBeVisible();

    // ── Type a message and send it ───────────────────────────────────────
    const input = sidebar.locator(".chat-input");
    await input.click();
    await input.fill(messageText);
    // Send button is enabled once the input is non-empty.
    const sendButton = sidebar.getByRole("button", { name: "Send message" });
    await expect(sendButton).toBeEnabled();
    await sendButton.click();

    // ── Verify the message is echoed into the message list ───────────────
    // do_send() optimistically appends a local ChatMessage with the typed
    // text, rendered inside a .chat-bubble.
    const bubble = sidebar.locator(".chat-bubble", { hasText: messageText });
    await expect(bubble).toBeVisible({ timeout: 10_000 });
    // The input should be cleared after sending.
    await expect(input).toHaveValue("");

    // ── Close the sidebar ────────────────────────────────────────────────
    await sidebar.getByRole("button", { name: "Close chat" }).click();
    await expect(sidebar).not.toHaveClass(/visible/, { timeout: 10_000 });

    // ── Reopen the sidebar ───────────────────────────────────────────────
    await page.getByRole("button", { name: "Open chat" }).click();
    await expect(sidebar).toHaveClass(/visible/, { timeout: 10_000 });

    // ── Verify the previously-sent message is still present ──────────────
    // The component stays mounted across the open/close toggle, so the local
    // message signal is preserved.
    await expect(sidebar.locator(".chat-bubble", { hasText: messageText })).toBeVisible({
      timeout: 10_000,
    });
  });
});
