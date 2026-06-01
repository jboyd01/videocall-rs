import { test, expect, Page } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * E2E coverage for the in-call chat sidebar.
 *
 * Flow exercised:
 *   1. Authenticate and join a meeting as a single user (host).
 *   2. Open the chat sidebar via the "Open Chat" video-control button.
 *   3. Type a message and press Enter.
 *   4. Verify the message bubble appears in the messages list.
 *   5. Close the sidebar via the "Close chat" button.
 *   6. Reopen the sidebar and verify the message is still visible
 *      (the messages signal lives in component state, which persists
 *      while the sidebar is hidden via CSS only).
 *
 * Selectors come from `dioxus-ui/src/components/chat_sidebar.rs` and
 * `dioxus-ui/src/components/video_control_buttons.rs::OpenChatButton`.
 */

const E2E_USER_EMAIL = "chat-e2e@videocall.rs";
const E2E_USER_NAME = "ChatTester";

async function joinMeetingFromPage(
  page: Page,
): Promise<"in-meeting" | "waiting" | "waiting-for-meeting"> {
  const joinButton = page.getByRole("button", { name: /Start Meeting|Join Meeting/ });
  const waitingRoom = page.getByText("Waiting to be admitted");
  const waitingForMeeting = page.getByText("Waiting for meeting to start");
  const grid = page.locator("#grid-container");

  const result = await Promise.race([
    joinButton.waitFor({ timeout: 30_000 }).then(() => "join" as const),
    waitingRoom.waitFor({ timeout: 30_000 }).then(() => "waiting" as const),
    waitingForMeeting.waitFor({ timeout: 30_000 }).then(() => "waiting-for-meeting" as const),
    grid.waitFor({ timeout: 30_000 }).then(() => "auto-joined" as const),
  ]);

  if (result === "waiting") return "waiting";
  if (result === "waiting-for-meeting") return "waiting-for-meeting";
  if (result === "auto-joined") return "in-meeting";

  await page.waitForTimeout(1000);
  await joinButton.click();
  await page.waitForTimeout(2000);
  await expect(grid).toBeVisible({ timeout: 15_000 });
  return "in-meeting";
}

async function gotoMeeting(page: Page, meetingId: string, displayName: string): Promise<void> {
  await page.goto("/");
  await page.waitForTimeout(1500);

  await page.locator("#meeting-id").click();
  await page.locator("#meeting-id").pressSequentially(meetingId, { delay: 50 });

  // Display name is a controlled input — clear before typing in case of pre-fill.
  await page.locator("#username").click();
  await page.locator("#username").fill("");
  await page.locator("#username").pressSequentially(displayName, { delay: 50 });
  await page.waitForTimeout(500);
  await page.locator("#username").press("Enter");

  await expect(page).toHaveURL(new RegExp(`/meeting/${meetingId}`), { timeout: 10_000 });
  await page.waitForTimeout(1500);
}

test.describe("Chat sidebar in-meeting", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL }) => {
    await injectSessionCookie(context, {
      baseURL,
      email: E2E_USER_EMAIL,
      name: E2E_USER_NAME,
    });
  });

  test("open → send message → close → reopen keeps the message visible", async ({ page }) => {
    const meetingId = `e2e_chat_${Date.now()}`;
    const messageText = `hello-from-e2e-${Date.now()}`;

    await gotoMeeting(page, meetingId, E2E_USER_NAME);
    const result = await joinMeetingFromPage(page);
    expect(result).toBe("in-meeting");

    // ---- Locate chat UI elements (selectors mirror the Rust component) ----
    const openChatButton = page.getByRole("button", { name: "Open Chat" });
    const sidebar = page.locator("#chat-sidebar");
    const closeButton = page.getByRole("button", { name: "Close chat" });
    const input = page.locator(".chat-input");
    const messagesList = page.locator("#chat-messages-list");

    // The sidebar is always mounted; its visible state is toggled via the
    // `visible` CSS class. Use that as the open/closed signal.
    await expect(sidebar).toHaveCount(1);
    await expect(sidebar).not.toHaveClass(/visible/);

    // ---- 1. OPEN ---------------------------------------------------------
    await expect(openChatButton).toBeVisible({ timeout: 10_000 });
    await openChatButton.click();
    await expect(sidebar).toHaveClass(/visible/, { timeout: 5_000 });
    await expect(input).toBeVisible();

    // ---- 2. TYPE + SEND --------------------------------------------------
    await input.click();
    await input.fill(messageText);
    await input.press("Enter");

    // ---- 3. VERIFY MESSAGE APPEARS --------------------------------------
    // The optimistic local message is added immediately with class
    // `chat-message--self`; if the server round-trip succeeds, it is later
    // replaced by the server-fetched copy. Either way, a bubble carrying
    // the typed text must be visible.
    const bubbleWithText = messagesList.locator(".chat-bubble", { hasText: messageText });
    await expect(bubbleWithText.first()).toBeVisible({ timeout: 10_000 });

    // The input should be cleared after sending.
    await expect(input).toHaveValue("");

    // ---- 4. CLOSE --------------------------------------------------------
    await closeButton.click();
    await expect(sidebar).not.toHaveClass(/visible/, { timeout: 5_000 });

    // ---- 5. REOPEN AND VERIFY MESSAGE PERSISTS --------------------------
    await openChatButton.click();
    await expect(sidebar).toHaveClass(/visible/, { timeout: 5_000 });
    await expect(bubbleWithText.first()).toBeVisible({ timeout: 5_000 });
  });
});
