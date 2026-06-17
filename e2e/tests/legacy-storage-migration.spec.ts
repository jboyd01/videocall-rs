import { test, expect } from "@playwright/test";
import { injectSessionCookie } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

/**
 * E2E tests for `migrate_legacy_storage()` in `dioxus-ui/src/context.rs`.
 *
 * The function runs once in `main()` before the component tree mounts. It
 * migrates old localStorage formats to the current plain-text
 * `vc_display_name` key:
 *
 *   1. If `vc_display_name` exists and looks like a CBOR hex blob (even
 *      length, all hex chars, >= 4 chars), it is removed.
 *   2. If `vc_display_name_raw` exists, its value is promoted to
 *      `vc_display_name` and the raw key is removed.
 *   3. Otherwise, if `vc_username` exists, its value is promoted to
 *      `vc_display_name` and the legacy key is removed.
 *   4. If `vc_display_name` is already plain text, it is left untouched.
 */
test.describe("Legacy storage migration (migrate_legacy_storage)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test.beforeEach(async ({ context, baseURL }) => {
    await injectSessionCookie(context, { baseURL });
  });

  test.afterEach(async ({ page }) => {
    await page.evaluate(() => {
      localStorage.removeItem("vc_display_name");
      localStorage.removeItem("vc_display_name_raw");
      localStorage.removeItem("vc_username");
    });
  });

  // 1. CBOR hex blob is detected and removed; no fallback keys exist so
  //    vc_display_name ends up absent and the #username input is empty.
  test("removes CBOR hex blob from vc_display_name when no fallback keys exist", async ({
    page,
  }) => {
    await page.goto("/");
    // "78064d79446f67" is valid CBOR encoding of "MyDog" — even length, all hex chars.
    await page.evaluate(() => localStorage.setItem("vc_display_name", "78064d79446f67"));
    await page.reload();

    await expect
      .poll(() => page.evaluate(() => localStorage.getItem("vc_display_name")), {
        timeout: 10_000,
      })
      .toBeNull();

    const input = page.locator("#username");
    await expect(input).toHaveValue("");
  });

  // 2. vc_display_name_raw is promoted to vc_display_name when no current
  //    key exists.
  test("promotes vc_display_name_raw to vc_display_name", async ({ page }) => {
    await page.goto("/");
    await page.evaluate(() => localStorage.setItem("vc_display_name_raw", "Alice"));
    await page.reload();

    await expect
      .poll(() => page.evaluate(() => localStorage.getItem("vc_display_name")), {
        timeout: 10_000,
      })
      .toBe("Alice");

    await expect
      .poll(() => page.evaluate(() => localStorage.getItem("vc_display_name_raw")), {
        timeout: 10_000,
      })
      .toBeNull();
  });

  // 3. vc_username is promoted to vc_display_name when no other keys exist.
  test("promotes vc_username to vc_display_name", async ({ page }) => {
    await page.goto("/");
    await page.evaluate(() => localStorage.setItem("vc_username", "Bob"));
    await page.reload();

    await expect
      .poll(() => page.evaluate(() => localStorage.getItem("vc_display_name")), {
        timeout: 10_000,
      })
      .toBe("Bob");

    await expect
      .poll(() => page.evaluate(() => localStorage.getItem("vc_username")), {
        timeout: 10_000,
      })
      .toBeNull();
  });

  // 4. When a CBOR hex blob exists alongside vc_display_name_raw, the blob
  //    is dropped and the raw fallback wins.
  test("drops CBOR hex blob and falls back to vc_display_name_raw", async ({ page }) => {
    await page.goto("/");
    await page.evaluate(() => {
      localStorage.setItem("vc_display_name", "78064d79446f67");
      localStorage.setItem("vc_display_name_raw", "Alice");
    });
    await page.reload();

    await expect
      .poll(() => page.evaluate(() => localStorage.getItem("vc_display_name")), {
        timeout: 10_000,
      })
      .toBe("Alice");

    await expect
      .poll(() => page.evaluate(() => localStorage.getItem("vc_display_name_raw")), {
        timeout: 10_000,
      })
      .toBeNull();
  });

  // 5. A genuine plain-text value in vc_display_name is left untouched.
  test("preserves plain-text vc_display_name without modification", async ({ page }) => {
    await page.goto("/");
    await page.evaluate(() => localStorage.setItem("vc_display_name", "RealName"));
    await page.reload();

    await expect
      .poll(() => page.evaluate(() => localStorage.getItem("vc_display_name")), {
        timeout: 10_000,
      })
      .toBe("RealName");
  });
});
