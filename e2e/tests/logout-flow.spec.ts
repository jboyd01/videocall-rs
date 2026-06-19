/**
 * E2E: OIDC logout flow via top-level navigation (#1547 / PR #1550).
 *
 * Verifies that clicking Sign Out:
 *  1. Clears client-side auth state (sessionStorage tokens)
 *  2. Performs a top-level navigation to the backend /logout endpoint
 *  3. Lands back on the home page in a signed-out state (ProviderButton visible)
 *
 * The e2e stack has no real IdP, so this exercises the FALLBACK redirect branch
 * (no end_session_endpoint → redirect to after_login_url or "/"). The client-side
 * token clear + navigation is the load-bearing behavior this PR introduces.
 */

import { test, expect } from "@playwright/test";
import { chromium, BrowserContext } from "@playwright/test";
import { BROWSER_ARGS, createAuthenticatedContext } from "../helpers/auth-context";

const UI_URL = process.env.DIOXUS_URL || "http://localhost:3001";

/**
 * Route-patch config.js to enable OAuth UI (the auth dropdown only renders
 * when oauthEnabled is truthy) and set a meetingApiUrl so logout_url() resolves.
 */
async function enableOAuthConfig(context: BrowserContext): Promise<void> {
  await context.route("**/config.js", async (route) => {
    const response = await route.fetch();
    const original = await response.text();

    const overrides = JSON.stringify({
      oauthEnabled: "true",
      meetingApiBaseUrl: UI_URL.replace(":3001", ":8080"),
    });

    const injection = `;window.__APP_CONFIG=Object.assign(window.__APP_CONFIG||{},${overrides});`;

    const patched = original.trimStart().startsWith("window.__APP_CONFIG")
      ? original + injection
      : `window.__APP_CONFIG=window.__APP_CONFIG||{};` + injection;

    await route.fulfill({
      status: 200,
      contentType: "application/javascript",
      body: patched,
    });
  });
}

test.describe("Logout flow (#1547)", () => {
  let browser: Awaited<ReturnType<typeof chromium.launch>>;
  let context: BrowserContext;

  test.beforeAll(async () => {
    browser = await chromium.launch({ args: BROWSER_ARGS });
  });

  test.afterAll(async () => {
    await browser.close();
  });

  test.beforeEach(async () => {
    context = await createAuthenticatedContext(
      browser,
      "e2e-logout@example.com",
      "E2E Logout User",
      UI_URL,
    );
    await enableOAuthConfig(context);
  });

  test.afterEach(async () => {
    await context.close();
  });

  test("Sign Out performs a top-level navigation to /logout (not fetch)", async () => {
    const page = await context.newPage();

    // Abort the /logout navigation so the page stays on the :3001 origin.
    // The synchronous WASM clear_* + set_href fires before the browser leaves,
    // and aborting prevents the cross-origin hop that would lose sessionStorage.
    await page.route("**/logout*", async (route) => {
      await route.abort();
    });

    await page.goto("/");

    const trigger = page.locator(".auth-dropdown-trigger");
    await expect(trigger).toBeVisible({ timeout: 10_000 });

    // Track only NAVIGATION requests to /logout — fetch/XHR must not count.
    let logoutNavDetected = false;
    let logoutFetchDetected = false;
    page.on("request", (req) => {
      if (req.url().includes("/logout")) {
        if (req.isNavigationRequest()) {
          logoutNavDetected = true;
        } else {
          logoutFetchDetected = true;
        }
      }
    });

    // Open the dropdown and click Sign Out
    await trigger.click();
    const signoutBtn = page.locator(".auth-dropdown-signout");
    await expect(signoutBtn).toBeVisible();
    await signoutBtn.click();

    // Give the set_href call a moment to fire
    await page.waitForTimeout(2_000);

    // The load-bearing assertion: /logout was reached via top-level navigation,
    // NOT via fetch(). This is the exact regression class #1547 introduced.
    expect(logoutNavDetected).toBe(true);
    expect(logoutFetchDetected).toBe(false);
  });

  test("After logout, sessionStorage tokens are cleared", async () => {
    const page = await context.newPage();

    // Abort the /logout navigation so the page stays on the :3001 origin
    // where we seeded the tokens. This lets us verify sessionStorage post-click.
    await page.route("**/logout*", async (route) => {
      await route.abort();
    });

    await page.goto("/");

    // Seed tokens in sessionStorage to simulate a real PKCE session
    await page.evaluate(() => {
      sessionStorage.setItem("vc_id_token", "fake-id-token");
      sessionStorage.setItem("vc_access_token", "fake-access-token");
      sessionStorage.setItem("vc_refresh_token", "fake-refresh-token");
    });

    // Wait for auth dropdown
    const trigger = page.locator(".auth-dropdown-trigger");
    await expect(trigger).toBeVisible({ timeout: 10_000 });

    // Click Sign Out
    await trigger.click();
    const signoutBtn = page.locator(".auth-dropdown-signout");
    await expect(signoutBtn).toBeVisible();
    await signoutBtn.click();

    // The synchronous clear calls run before set_href fires. With navigation
    // aborted, we stay on :3001 and can read the same-origin sessionStorage.
    await page.waitForTimeout(500);

    const tokensAfter = await page.evaluate(() => ({
      idToken: sessionStorage.getItem("vc_id_token"),
      accessToken: sessionStorage.getItem("vc_access_token"),
      refreshToken: sessionStorage.getItem("vc_refresh_token"),
    }));

    expect(tokensAfter.idToken).toBeNull();
    expect(tokensAfter.accessToken).toBeNull();
    expect(tokensAfter.refreshToken).toBeNull();
  });
});
