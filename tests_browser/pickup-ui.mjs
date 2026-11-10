// The real Rust server owns all state and wire contracts. The carrier and
// tracking provider behind it are disposable in-process fixtures, never live.
import { pathToFileURL } from "node:url";
import { mkdir } from "node:fs/promises";
const { chromium, expect } = await import(
  pathToFileURL(process.env.PACKAGES_PLAYWRIGHT_MODULE).href
);
const variant = process.env.PACKAGES_TEST_VARIANT;
const mobile = variant === "mobile-dark";
const screenshots = process.env.PACKAGES_SCREENSHOT_DIR;
if (screenshots) await mkdir(screenshots, { recursive: true });
const browser = await chromium.launch();
try {
  const context = await browser.newContext({
    viewport: mobile
      ? { width: 390, height: 844 }
      : { width: 1440, height: 1000 },
    colorScheme: mobile ? "dark" : "light",
  });
  const page = await context.newPage();
  page.setDefaultTimeout(10000);
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  const snapshot = async (name) => {
    expect(
      await page.evaluate(
        () => document.documentElement.scrollWidth <= innerWidth,
      ),
      "The page must not scroll sideways",
    ).toBe(true);
    if (screenshots)
      await page.screenshot({
        path: `${screenshots}/${variant}-${name}.png`,
        fullPage: true,
      });
  };
  await page.goto(process.env.PACKAGES_TEST_URL);
  await page.locator("#login").waitFor({ state: "visible" });
  await page.locator("#login input").fill(process.env.PACKAGES_TEST_OWNER);
  await page.locator("#login button").click();
  await expect(page.getByRole("link", { name: "Packages home" })).toBeVisible();
  await expect(page.locator("#app-version")).toHaveText(
    process.env.PACKAGES_TEST_VERSION,
  );
  await page.getByRole("button", { name: "Add shipment", exact: true }).click();
  await expect(page.locator("#editor")).toBeVisible();
  await expect(
    page.locator("#editor .dialog-footer button#save"),
  ).toBeInViewport({ ratio: 1 });
  await snapshot("package-editor");
  await page.locator('#editor button[data-close="editor"]').last().click();
  await page.getByRole("button", { name: "Pickups", exact: true }).click();
  await expect(
    page.getByRole("heading", { name: "Pickups", exact: true }),
  ).toBeVisible();
  await expect(page.locator("#pickup-site-summary")).toContainText(
    "123 Example Street",
  );
  await snapshot("list");
  await page
    .getByRole("button", { name: "Pickup address", exact: true })
    .click();
  await expect(page.getByLabel("Street address", { exact: true })).toHaveValue(
    "123 Example Street",
  );
  const saveAddress = page.getByRole("button", {
    name: "Save default pickup address",
    exact: true,
  });
  await expect(saveAddress).toBeInViewport({ ratio: 1 });
  await snapshot("address");
  await page
    .getByLabel("Residential address", { exact: true })
    .scrollIntoViewIfNeeded();
  await expect(page.locator("#pickup-site-title")).toBeInViewport({ ratio: 1 });
  await expect(page.locator("#pickup-site-close")).toBeInViewport({ ratio: 1 });
  await expect(saveAddress).toBeInViewport({ ratio: 1 });
  await snapshot("address-bottom");
  await page
    .getByLabel("Location name", { exact: true })
    .fill("Example site verified");
  await saveAddress.click();
  await expect(page.locator("#pickup-site-dialog")).toBeHidden();
  await expect(page.locator("#pickup-site-summary")).toContainText(
    "Example site verified",
  );
  await page
    .getByRole("button", { name: "Schedule pickup", exact: true })
    .click();
  await page.getByLabel("Prepaid tracking numbers").fill("TESTPICKUP123");
  await page
    .locator('#pickup-schedule-form select[name="carrier"]')
    .selectOption("ups");
  await snapshot("schedule");
  await page
    .getByRole("button", { name: "Review pickup", exact: true })
    .click();
  await expect(page.locator("#pickup-review-details")).toContainText(
    "123 Example Street",
  );
  await expect(page.locator("#pickup-review-details")).toContainText(
    "Fee not quoted.",
  );
  await expect(page.locator("#pickup-review-details")).toContainText("2.50 lb");
  await expect(page.locator("#pickup-schedule-title")).toBeInViewport({
    ratio: 1,
  });
  await expect(page.locator("#pickup-schedule-close")).toBeInViewport({
    ratio: 1,
  });
  await expect(page.locator("#pickup-schedule-commit")).toBeInViewport({
    ratio: 1,
  });
  await snapshot("review");
  let schedules = 0;
  let releaseSchedule;
  const held = new Promise((resolve) => {
    releaseSchedule = resolve;
  });
  await page.route("**/v1/pickups", async (route) => {
    if (route.request().method() !== "POST") return route.continue();
    schedules++;
    expect(route.request().postDataJSON().accept_fee).toBe(true);
    const response = await route.fetch();
    await held;
    await route.fulfill({ response });
  });
  await page.keyboard.press("Enter");
  expect(schedules, "Enter on a review must not schedule").toBe(0);
  await page.locator("#pickup-schedule-commit").click();
  await expect(
    page.getByRole("checkbox", {
      name: "Accept whatever pickup fee",
      exact: false,
    }),
  ).toBeFocused();
  expect(schedules, "Unaccepted charges must not schedule").toBe(0);
  await page
    .getByRole("checkbox", { name: "Accept whatever pickup fee", exact: false })
    .check();
  await page.locator("#pickup-schedule-commit").click();
  await expect.poll(() => schedules).toBe(1);
  await page.keyboard.press("Escape");
  await page.keyboard.press("Escape");
  await expect(page.locator("#pickup-schedule-dialog")).toBeVisible();
  releaseSchedule();
  await expect(
    page.getByText("Confirmation: PICKUP-FIXTURE", { exact: true }),
  ).toBeVisible();
  await expect(
    page.getByText(
      "Pickup booked. Physical collection has not been confirmed.",
      { exact: true },
    ),
  ).toBeVisible();
  expect(schedules).toBe(1);
  await snapshot("confirmation");
  let cancellations = 0;
  await page.route("**/v1/pickups/records/*/cancel", (route) => {
    cancellations++;
    return route.continue();
  });
  await page
    .getByRole("button", { name: "Cancel pickup", exact: true })
    .click();
  await expect(
    page.getByRole("heading", { name: "Cancel pickup?", exact: true }),
  ).toBeVisible();
  await snapshot("cancel-review");
  await page.getByRole("button", { name: "Keep pickup", exact: true }).click();
  await expect(
    page.getByText("Confirmation: PICKUP-FIXTURE", { exact: true }),
  ).toBeVisible();
  expect(cancellations).toBe(0);
  await page.reload();
  await page.locator("#login").waitFor({ state: "visible" });
  await page.locator("#login input").fill(process.env.PACKAGES_TEST_OWNER);
  await page.locator("#login button").click();
  await page.getByRole("button", { name: "Pickups", exact: true }).click();
  await expect(
    page.getByText("Confirmation: PICKUP-FIXTURE", { exact: true }),
  ).toBeVisible();
  expect(schedules).toBe(1);
  expect(cancellations).toBe(0);
  await page
    .getByRole("button", { name: "Back to packages", exact: true })
    .click();
  await page.route("**/v1/packages?*", (route) =>
    route.fulfill({
      status: 503,
      json: { message: "Fixture connection unavailable" },
    }),
  );
  await page
    .getByRole("button", { name: "Refresh shipment list", exact: true })
    .click();
  await expect(page.locator("#notice")).toContainText(
    "Fixture connection unavailable",
  );
  await expect(page.locator("#notice")).toHaveClass(/error/);
  await snapshot("connection-error");
  expect(errors).toEqual([]);
  await context.close();
} finally {
  await browser.close();
}
