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
  await snapshot("package-search");
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
  await page.getByText("Pickup settings", { exact: true }).click();
  await page
    .getByRole("button", { name: "Edit pickup address: Example site", exact: true })
    .click();
  await expect(page.getByLabel("Street address", { exact: true })).toHaveValue(
    "123 Example Street",
  );
  await expect(page.locator('#pickup-site-form input[name="email"]')).not.toHaveAttribute("required");
  const saveAddress = page.getByRole("button", {
    name: "Save address",
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
    .getByLabel("Address label", { exact: false })
    .fill("Example site verified");
  await saveAddress.click();
  await expect(page.locator("#pickup-site-dialog")).toBeHidden();
  await expect(page.locator("#pickup-addresses")).toContainText(
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
      name: "I accept any pickup fee",
      exact: false,
    }),
  ).toBeFocused();
  expect(schedules, "Unaccepted charges must not schedule").toBe(0);
  await page
    .getByRole("checkbox", { name: "I accept any pickup fee", exact: false })
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
  await setupPage.clock.install();
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
  await setupPage.route("**/v1/pickups/verify-address", (route) => route.fulfill({ json: null }));
  await setupPage.goto(process.env.PACKAGES_TEST_URL);
  await setupPage.locator("#login input").fill(process.env.PACKAGES_TEST_OWNER);
  await setupPage.locator("#login button").click();
  await setupPage.getByRole("button", { name: "Pickups", exact: true }).click();
  await expect(setupPage.getByRole("button", { name: "Set up pickup address", exact: true })).toBeVisible();
  await setupPage.getByRole("button", { name: "Outbound", exact: true }).click();
  await setupPage.locator("#list .shipment").filter({ hasText: "TESTPICKUP123" }).click();
  await setupPage.locator("#actions").getByRole("button", { name: "Schedule pickup", exact: true }).click();
  await expect(setupPage.locator("#pickup-site-dialog")).toBeVisible();
  await expect(setupPage.getByRole("heading", { name: "Set up pickup address", exact: true })).toBeVisible();
  await expect(setupPage.locator('#pickup-site-form [name="name"]')).toBeHidden();
  await expect(setupPage.locator("#dashboard")).toBeVisible();
  await expect(setupPage.getByLabel("Time zone", { exact: true })).toHaveValue("Pacific/Honolulu");
  for (const [name, value] of Object.entries({ contact_name: "Example Contact", phone: "2025550123", street: "123 Example Street", city: "Example City", postal_code: "90000" }))
    await setupPage.locator(`#pickup-site-form [name="${name}"]`).fill(value);
  await setupPage.locator('#pickup-site-form select[name="state"]').selectOption({ label: "California" });
  await setupPage.getByRole("button", { name: "Save default address", exact: true }).click();
  await expect(setupPage.locator("#pickup-site-dialog")).toBeHidden();
  await expect(setupPage.locator("#pickup-schedule-dialog")).toBeVisible();
  await expect(setupPage.getByLabel("Prepaid tracking numbers")).toHaveValue("TESTPICKUP123");
  await expect(setupPage.locator('#pickup-schedule-form input[name="date"]')).toHaveValue(
    new Intl.DateTimeFormat("sv-SE", { timeZone: "Pacific/Honolulu" }).format(new Date()),
  );
  await expect(setupPage.locator("#dashboard")).toBeVisible();
  await setupPage.locator("#pickup-schedule-close").click();

  await setupPage.getByRole("button", { name: "Pickups", exact: true }).click();
  await expect(setupPage.getByRole("button", { name: "Set up pickup address", exact: true })).toBeHidden();
  await setupPage.getByText("Pickup settings", { exact: true }).click();
  await setupPage.getByRole("button", { name: "Add another address", exact: true }).click();
  await setupPage.unroute("**/v1/pickups/verify-address");
  let validations = 0;
  await setupPage.route("**/v1/pickups/verify-address", (route) => {
    validations++;
    const address = route.request().postDataJSON();
    expect(Object.keys(address).sort()).toEqual(["city", "country", "postal_code", "secondary", "state", "street"]);
    return route.fulfill({ json: { status: "verified", normalized: { ...address, street: "42 EXAMPLE AVE" } } });
  });
  for (const [name, value] of Object.entries({ name: "Office", contact_name: "Example Contact", phone: "2025550123", street: "42 Example Avenue", city: "Example City", postal_code: "90000" }))
    await setupPage.locator(`#pickup-site-form [name="${name}"]`).fill(value);
  await setupPage.locator('#pickup-site-form select[name="state"]').selectOption("CA");
  await setupPage.getByRole("button", { name: "Save address", exact: true }).click();
  await expect(setupPage.locator("#pickup-address-review")).toContainText("42 EXAMPLE AVE");
  await expect(setupPage.locator("#pickup-addresses")).not.toContainText("Office");
  await setupPage.getByRole("button", { name: "Use suggested address", exact: true }).click();
  await expect(setupPage.locator("#pickup-site-dialog")).toBeHidden();
  await expect(setupPage.locator("#pickup-site-summary")).not.toContainText("42 EXAMPLE AVE");
  expect(validations).toBe(1);
  await setupPage.getByRole("button", { name: "Make Office the default pickup address", exact: true }).click();
  await expect(setupPage.locator("#pickup-site-summary")).toContainText("42 EXAMPLE AVE");
  await setupPage.getByRole("button", { name: "Schedule pickup", exact: true }).click();
  await expect(setupPage.locator('#pickup-schedule-form select[name="site_id"] option:checked')).toContainText("Office");
  const home = setupPage.locator('#pickup-schedule-form select[name="site_id"] option').filter({ hasText: "Home" });
  await setupPage.locator('#pickup-schedule-form select[name="site_id"]').selectOption(await home.getAttribute("value"));
  await expect(setupPage.locator('#pickup-schedule-form select[name="site_id"] option:checked')).toContainText("Home");
  await setupPage.locator("#pickup-schedule-close").click();

  // A provider response from the previous address dialog cannot save over the new one.
  await setupPage.unroute("**/v1/pickups/verify-address");
  let startVerification;
  const verificationStarted = new Promise((resolve) => { startVerification = resolve; });
  let releaseVerification;
  const heldVerification = new Promise((resolve) => { releaseVerification = resolve; });
  await setupPage.route("**/v1/pickups/verify-address", async (route) => {
    startVerification();
    await heldVerification;
    await route.fulfill({ json: { status: "verified", normalized: null } });
  });
  await setupPage.getByRole("button", { name: "Edit pickup address: Home", exact: true }).click();
  await setupPage.getByLabel("Street address", { exact: true }).fill("99 Old Example Road");
  await setupPage.getByRole("button", { name: "Save address", exact: true }).click();
  await verificationStarted;
  await setupPage.route("**/v1/pickups/carriers", async (route) => {
    const response = await route.fetch();
    const carriers = await response.json();
    await route.fulfill({ response, json: carriers.map((carrier) => carrier.configured
      ? { ...carrier, configuration_id: "00000000-0000-4000-8000-000000000001" } : carrier) });
  });
  await setupPage.clock.fastForward(31_000);
  await expect(setupPage.locator("#pickup-site-dialog")).toBeHidden();
  await setupPage.getByRole("button", { name: "Edit pickup address: Office", exact: true }).click();
  const response = setupPage.waitForResponse("**/v1/pickups/verify-address");
  releaseVerification();
  await response;
  await expect(setupPage.getByLabel("Street address", { exact: true })).toHaveValue("42 EXAMPLE AVE");
  await setupPage.getByLabel("Address label", { exact: false }).fill("Example Office");
  await setupPage.getByRole("button", { name: "Save address", exact: true }).click();
  await expect(setupPage.locator("#pickup-addresses")).toContainText("Example Office");
  await expect(setupPage.locator("#pickup-addresses")).not.toContainText("99 Old Example Road");
  await setupPage.unroute("**/v1/pickups/carriers");
  await setupPage.getByRole("button", { name: "Outbound", exact: true }).click();

  // Losing optional carrier credentials removes every scheduling entry point.
  await setupPage.route("**/v1/pickups/carriers", async (route) => {
    const response = await route.fetch();
    const carriers = await response.json();
    await route.fulfill({ response, json: carriers.map((carrier) => ({ ...carrier, configured: false })) });
  });
  let unusablePreviews = 0;
  setupPage.on("request", (request) => {
    if (new URL(request.url()).pathname === "/v1/pickups/preview") unusablePreviews++;
  });
  await setupPage.getByRole("button", { name: "Refresh package list", exact: true }).click();
  await expect(setupPage.locator("#pickup-schedule-dialog")).toBeHidden();
  await expect(setupPage.locator("#schedule-pickup")).toBeHidden();
  await expect(setupPage.locator("#show-pickups")).toBeHidden();
  expect(unusablePreviews).toBe(0);
  await setupPage.unroute("**/v1/pickups/carriers");
  await setupPage.getByRole("button", { name: "Refresh package list", exact: true }).click();
  await expect(setupPage.locator("#schedule-pickup")).toBeVisible();

  // A permission change removes both entry points even if a refresh races it.
  await setupPage.route("**/v1/pickups/carriers", (route) => route.fulfill({ status: 403, json: { message: "Owner access required" } }));
  await setupPage.getByRole("button", { name: "Refresh package list", exact: true }).click();
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
