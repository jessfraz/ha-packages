// Native status responses come from real zero/one/multiple-provider servers.
// Switching the status origin models a restart without fabricating capabilities.
import { pathToFileURL } from "node:url";
import { mkdir } from "node:fs/promises";
const { chromium, expect } = await import(pathToFileURL(process.env.PACKAGES_PLAYWRIGHT_MODULE).href);
const one = process.env.PACKAGES_TEST_URL;
const zero = process.env.PACKAGES_TEST_ZERO_URL;
const multiple = process.env.PACKAGES_TEST_MULTIPLE_URL;
const rotated = process.env.PACKAGES_TEST_ROTATED_URL;
const owner = process.env.PACKAGES_TEST_OWNER;
const agent = process.env.PACKAGES_TEST_AGENT;
const browser = await chromium.launch({ executablePath: process.env.PACKAGES_CHROMIUM_EXECUTABLE });
const screenshots = process.env.PACKAGES_SCREENSHOT_DIR;
if (screenshots) await mkdir(screenshots, { recursive: true });
const snapshot = async (page, name) => {
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  if (screenshots) await page.screenshot({ path: `${screenshots}/${name}.png`, fullPage: true });
};
const login = async (page, origin, token = owner) => {
  page.setDefaultTimeout(10000);
  await page.goto(origin);
  await page.locator("#login input").fill(token);
  await page.locator("#login button").click();
  await page.getByRole("button", { name: "All", exact: true }).click();
  await expect(page.locator("#list")).toHaveAttribute("aria-busy", "false");
};
const entryPoints = async (page, available) => {
  for (const id of ["schedule-pickup", "show-pickups"])
    if (available) await expect(page.locator(`#${id}`)).toBeVisible();
    else await expect(page.locator(`#${id}`)).toBeHidden();
};
const details = async (page, name) => {
  await page.locator("#list .shipment").filter({ hasText: name }).click();
  await expect(page.locator("#detail")).toBeVisible();
};
const options = (page) => page.locator('#pickup-schedule-form select[name="carrier"] option').evaluateAll((items) => items.map((item) => item.value));
try {
  const zeroPage = await browser.newPage();
  await login(zeroPage, zero);
  await entryPoints(zeroPage, false);
  await snapshot(zeroPage, "zero-configured-carriers");
  await details(zeroPage, "USPS capability fixture");
  await expect(zeroPage.locator("#actions").getByRole("button", { name: "Schedule pickup", exact: true })).toBeHidden();
  await zeroPage.goto(`${zero}/#page=pickups`);
  await zeroPage.reload();
  await zeroPage.locator("#login input").fill(owner);
  await zeroPage.locator("#login button").click();
  await expect(zeroPage.locator("#dashboard")).toBeVisible();
  await expect(zeroPage.locator("#pickups-dashboard")).toBeHidden();
  await zeroPage.close();

  const page = await browser.newPage();
  await page.clock.install();
  const errors = [];
  let statusOrigin = one;
  let deny = false;
  let admissions = 0;
  page.on("pageerror", (error) => errors.push(error.message));
  page.on("request", (request) => {
    if (request.method() === "POST" && new URL(request.url()).pathname === "/v1/pickups") admissions++;
  });
  await page.route("**/v1/pickups/carriers", async (route) => {
    const headers = { ...route.request().headers(), authorization: `Bearer ${deny ? agent : owner}` };
    const response = await route.fetch({ url: new URL("/v1/pickups/carriers", statusOrigin).href, headers });
    await route.fulfill({ response });
  });
  await login(page, one);
  await entryPoints(page, true);
  await page.locator("#show-pickups").click();
  await expect(page.locator("#pickups-dashboard")).toBeVisible();
  await expect(page.getByRole("list", { name: "Enabled pickup carriers" }).getByRole("listitem")).toHaveText(["UPS"]);
  await expect(page.locator("#pickup-records")).toContainText("No pickup requests yet.");
  statusOrigin = zero;
  await page.clock.fastForward(31_000);
  await expect(page.locator("#dashboard")).toBeVisible();
  await expect(page.locator("#pickups-dashboard")).toBeHidden();
  await entryPoints(page, false);
  expect(new URL(page.url()).hash).not.toContain("page=pickups");
  statusOrigin = one;
  await page.getByRole("button", { name: "Refresh package list", exact: true }).click();
  await entryPoints(page, true);
  await expect(page.locator("#list")).toHaveAttribute("aria-busy", "false");
  await details(page, "USPS capability fixture");
  await expect(page.locator("#actions").getByRole("button", { name: "Schedule pickup", exact: true })).toBeHidden();
  await page.keyboard.press("Escape");
  await details(page, "Incoming UPS label");
  await expect(page.locator("#actions").getByRole("button", { name: "Schedule pickup", exact: true })).toBeHidden();
  await page.keyboard.press("Escape");
  await details(page, "UPS capability fixture");
  await page.locator("#actions").getByRole("button", { name: "Schedule pickup", exact: true }).click();
  await expect(page.locator("#pickup-schedule-dialog")).toBeVisible();
  expect(await options(page)).toEqual(["", "ups"]);
  const carrier = page.locator('#pickup-schedule-form select[name="carrier"]');
  await carrier.selectOption("ups");
  await snapshot(page, "single-configured-carrier");
  expect(await page.locator('#pickup-schedule-form select[name="service"] option').evaluateAll((items) => items.every((item) => !item.value || item.value.startsWith("ups_")))).toBe(true);
  await page.getByRole("button", { name: "Review pickup", exact: true }).click();
  await expect(page.locator("#pickup-review")).toBeVisible();
  await page.getByRole("checkbox", { name: "I accept any pickup fee" }).check();
  statusOrigin = zero;
  await page.locator("#pickup-schedule-commit").click();
  await expect(page.locator("#pickup-schedule-dialog")).toBeHidden();
  await entryPoints(page, false);
  expect(admissions).toBe(0);

  statusOrigin = multiple;
  await page.getByRole("button", { name: "Refresh package list", exact: true }).click();
  await entryPoints(page, true);
  await expect(page.locator("#list")).toHaveAttribute("aria-busy", "false");
  await page.locator("#schedule-pickup").click();
  await expect.poll(() => options(page)).toEqual(["", "usps", "ups", "fedex"]);
  await carrier.selectOption("fedex");
  expect(await page.locator('#pickup-schedule-form select[name="service"] option').evaluateAll((items) => items.every((item) => !item.value || item.value.startsWith("fedex_")))).toBe(true);
  statusOrigin = zero;
  await page.clock.fastForward(31_000);
  await expect(page.locator("#pickup-schedule-dialog")).toBeHidden();
  await entryPoints(page, false);
  await details(page, "UPS capability fixture");
  await expect(page.locator("#actions").getByRole("button", { name: "Schedule pickup", exact: true })).toBeHidden();
  await page.keyboard.press("Escape");

  statusOrigin = one;
  await page.reload();
  await page.locator("#login input").fill(owner);
  await page.locator("#login button").click();
  await entryPoints(page, true);
  await expect(page.locator("#list")).toHaveAttribute("aria-busy", "false");
  await page.locator("#schedule-pickup").click();
  await expect.poll(() => options(page)).toEqual(["", "ups"]);
  await page.getByLabel("Prepaid tracking numbers").fill("TESTPICKUP123");
  await carrier.selectOption("ups");
  await page.getByRole("button", { name: "Review pickup", exact: true }).click();
  await page.getByRole("checkbox", { name: "I accept any pickup fee" }).check();
  statusOrigin = rotated;
  await page.locator("#pickup-schedule-commit").click();
  await expect(page.locator("#pickup-schedule-dialog")).toBeHidden();
  await entryPoints(page, true);
  expect(admissions).toBe(0);
  await page.locator("#schedule-pickup").click();
  await expect(page.getByLabel("Prepaid tracking numbers")).toBeEnabled();
  deny = true;
  await page.clock.fastForward(31_000);
  await expect(page.locator("#pickup-schedule-dialog")).toBeHidden();
  await expect(page.locator("#pickup-site-dialog")).toBeHidden();
  await entryPoints(page, false);
  expect(admissions).toBe(0);
  expect(errors).toEqual([]);
  await page.close();

  const multiPage = await browser.newPage();
  await login(multiPage, multiple);
  await entryPoints(multiPage, true);
  await details(multiPage, "USPS capability fixture");
  await expect(multiPage.locator("#actions").getByRole("button", { name: "Schedule pickup", exact: true })).toBeVisible();
  await multiPage.keyboard.press("Escape");
  await multiPage.locator("#schedule-pickup").click();
  await expect.poll(() => options(multiPage)).toEqual(["", "usps", "ups", "fedex"]);
  await multiPage.locator('#pickup-schedule-form select[name="carrier"]').selectOption("fedex");
  await snapshot(multiPage, "multiple-configured-carriers");
  await multiPage.close();

  const agentPage = await browser.newPage();
  const privilegedRequests = [];
  agentPage.on("request", (request) => {
    if (new URL(request.url()).pathname.startsWith("/v1/pickups")) privilegedRequests.push(request.url());
  });
  await login(agentPage, one, agent);
  await entryPoints(agentPage, false);
  expect(privilegedRequests).toEqual([]);
  await agentPage.close();
} finally {
  await browser.close();
}
