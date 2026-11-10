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
const browser = await chromium.launch({ executablePath: process.env.PACKAGES_CHROMIUM_EXECUTABLE });
try {
  const context = await browser.newContext({
    viewport: mobile
      ? { width: 390, height: 844 }
      : { width: 1440, height: 1000 },
    colorScheme: mobile ? "dark" : "light",
  });
  const page = await context.newPage();
  page.setDefaultTimeout(10000);
  await page.clock.install();
  let packageReads = 0;
  page.on("request", (request) => {
    if (new URL(request.url()).pathname === "/v1/packages" && request.method() === "GET") packageReads++;
  });
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
  await page.getByRole("button", { name: "Pickups", exact: true }).click();
  await expect(page.locator("#directions")).toBeVisible();
  await expect(page.locator("#show-pickups")).toHaveAttribute("aria-current", "page");
  expect(new URL(page.url()).hash).toContain("page=pickups");
  await page.clock.fastForward(31_000);
  await page.getByRole("button", { name: "Inbound", exact: true }).click();
  await expect(page.locator("#dashboard")).toBeVisible();
  await expect(page.locator('#directions [data-direction="inbound"]')).toHaveAttribute("aria-current", "page");
  await page.goBack();
  await expect(page.locator("#pickups-dashboard")).toBeVisible();
  await page.goForward();
  await expect(page.locator("#dashboard")).toBeVisible();
  await expect(page.locator("#list")).not.toBeEmpty();
  await expect(page.locator("#list")).toHaveAttribute("aria-busy", "false");
  const readsBeforeRefresh = packageReads;
  await page.clock.fastForward(31_000);
  await expect.poll(() => packageReads).toBeGreaterThan(readsBeforeRefresh);
  await page.getByRole("button", { name: "Schedule pickup", exact: true }).click();
  await expect(page.locator("#pickup-schedule-dialog")).toBeVisible();
  await expect(page.locator("#dashboard")).toBeVisible();
  await expect(page.locator("#pickups-dashboard")).toBeHidden();
  await page.locator("#pickup-schedule-close").click();
  await page.getByRole("button", { name: "Pickups", exact: true }).click();
  await page.getByRole("button", { name: "Add package", exact: true }).click();
  await expect(page.locator("#editor")).toBeVisible();
  await expect(
    page.locator("#editor .dialog-footer button#save"),
  ).toBeInViewport({ ratio: 1 });
  await snapshot("package-editor");
  await page.reload();
  await page.locator("#login input").fill(process.env.PACKAGES_TEST_OWNER);
  await page.locator("#login button").click();
  await expect(page.locator("#editor")).toBeVisible();
  await expect(page.locator("#pickups-dashboard")).toBeVisible();
  await page.locator('#editor button[data-close="editor"]').last().click();
  await expect(
    page.getByRole("heading", { name: "Pickups", exact: true }),
  ).toBeVisible();
  await expect(page.locator("#pickup-site-summary")).toContainText(
    "123 Example Street",
  );
  await snapshot("list");
  await page
    .getByRole("button", { name: "Edit pickup address", exact: true })
    .click();
  await expect(page.getByLabel("Street address", { exact: true })).toHaveValue(
    "123 Example Street",
  );
  await expect(page.locator('#pickup-site-form input[name="email"]')).not.toHaveAttribute("required");
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
  await expect(page.locator("#show-pickups")).toHaveAttribute("aria-current", "page");
  await expect(
    page.getByText("Confirmation: PICKUP-FIXTURE", { exact: true }),
  ).toBeVisible();
  expect(schedules).toBe(1);
  expect(cancellations).toBe(0);
  await page
    .getByRole("button", { name: "Inbound", exact: true })
    .click();
  await page.getByRole("button", { name: "Outbound", exact: true }).click();
  await page.locator("#list .shipment").filter({ hasText: "TESTPICKUP123" }).click();
  await page.locator("#actions").getByRole("button", { name: "Schedule pickup", exact: true }).click();
  await expect(page.getByLabel("Prepaid tracking numbers")).toHaveValue("TESTPICKUP123");
  await expect(page.locator("#dashboard")).toBeVisible();
  await expect(page.locator("#pickups-dashboard")).toBeHidden();
  await page.locator("#pickup-schedule-close").click();
  await page.route("**/v1/packages?*", (route) =>
    route.fulfill({
      status: 503,
      json: { message: "Fixture connection unavailable" },
    }),
  );
  await page
    .getByRole("button", { name: "Refresh package list", exact: true })
    .click();
  await expect(page.locator("#notice")).toContainText(
    "Fixture connection unavailable",
  );
  await expect(page.locator("#notice")).toHaveClass(/error/);
  await snapshot("connection-error");
  expect(errors).toEqual([]);
  await context.close();

  // First-run setup uses the installation timezone even in a remote browser.
  const setupContext = await browser.newContext({ timezoneId: "UTC" });
  const setupPage = await setupContext.newPage();
  setupPage.setDefaultTimeout(8000);
  setupPage.on("pageerror", (error) => errors.push(error.message));
  await setupPage.route("**/v1/system", async (route) => {
    const response = await route.fetch();
    const system = await response.json();
    await route.fulfill({ response, json: { ...system, delivery_day: {
      date: "2026-11-10", time_zone: "Pacific/Honolulu",
      delivered_after: "2026-11-10T10:00:00Z", delivered_before: "2026-11-11T10:00:00Z",
    } } });
  });
  let initialSite = true;
  await setupPage.route("**/v1/pickups/locations", async (route) => {
    if (route.request().method() === "GET" && initialSite)
      return route.fulfill({ json: [] });
    initialSite = false;
    return route.continue();
  });
  await setupPage.goto(process.env.PACKAGES_TEST_URL);
  await setupPage.locator("#login input").fill(process.env.PACKAGES_TEST_OWNER);
  await setupPage.locator("#login button").click();
  await setupPage.getByRole("button", { name: "Pickups", exact: true }).click();
  await expect(setupPage.getByRole("button", { name: "Add pickup address", exact: true })).not.toHaveClass(/quiet/);
  await setupPage.getByRole("button", { name: "Outbound", exact: true }).click();
  await setupPage.locator("#list .shipment").filter({ hasText: "TESTPICKUP123" }).click();
  await setupPage.locator("#actions").getByRole("button", { name: "Schedule pickup", exact: true }).click();
  await expect(setupPage.locator("#pickup-site-dialog")).toBeVisible();
  await expect(setupPage.locator("#dashboard")).toBeVisible();
  await expect(setupPage.getByLabel("Time zone", { exact: true })).toHaveValue("Pacific/Honolulu");
  for (const [name, value] of Object.entries({ name: "Example collection", contact_name: "Example Contact", phone: "2025550123", street: "123 Example Street", city: "Example City", state: "CA", postal_code: "90000" }))
    await setupPage.locator(`#pickup-site-form [name="${name}"]`).fill(value);
  await setupPage.getByRole("button", { name: "Save default pickup address", exact: true }).click();
  await expect(setupPage.locator("#pickup-site-dialog")).toBeHidden();
  await expect(setupPage.locator("#pickup-schedule-dialog")).toBeVisible();
  await expect(setupPage.getByLabel("Prepaid tracking numbers")).toHaveValue("TESTPICKUP123");
  await expect(setupPage.locator("#dashboard")).toBeVisible();
  await setupPage.locator("#pickup-schedule-close").click();

  // No carrier credentials must be explained before any preview can be sent.
  await setupPage.route("**/v1/pickups/carriers", async (route) => {
    const response = await route.fetch();
    const carriers = await response.json();
    await route.fulfill({ response, json: carriers.map((carrier) => ({ ...carrier, configured: false })) });
  });
  let unusablePreviews = 0;
  setupPage.on("request", (request) => {
    if (new URL(request.url()).pathname === "/v1/pickups/preview") unusablePreviews++;
  });
  await setupPage.getByRole("button", { name: "Schedule pickup", exact: true }).click();
  await expect(setupPage.locator("#pickup-configuration")).toContainText("Connect a carrier account before scheduling");
  await expect(setupPage.getByRole("button", { name: "Review pickup", exact: true })).toBeDisabled();
  expect(unusablePreviews).toBe(0);
  await setupPage.locator("#pickup-schedule-close").click();
  await setupPage.unroute("**/v1/pickups/carriers");

  // A permission change closes body-level dialogs and removes both entry points.
  await setupPage.route("**/v1/pickups/carriers", (route) => route.fulfill({ status: 403, json: { message: "Owner access required" } }));
  await setupPage.getByRole("button", { name: "Schedule pickup", exact: true }).click();
  await expect(setupPage.locator("#pickup-schedule-dialog")).toBeHidden();
  await expect(setupPage.locator("#pickup-site-dialog")).toBeHidden();
  await expect(setupPage.locator("#show-pickups")).toBeHidden();
  await expect(setupPage.locator("#schedule-pickup")).toBeHidden();
  await expect(setupPage.locator("#dashboard")).toBeVisible();
  expect(errors).toEqual([]);
  await setupContext.close();
} finally {
  await browser.close();
}
