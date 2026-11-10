// Opt-in browser regression against the actual Rust service, with two distinct
// authorized principals. All credentials and addresses are disposable fixtures.
import { pathToFileURL } from "node:url";
const { chromium, expect } = await import(
  pathToFileURL(process.env.PACKAGES_PLAYWRIGHT_MODULE).href
);
const origin = process.env.PACKAGES_TEST_URL;
const owner = process.env.PACKAGES_TEST_OWNER;
const secondAdmin = process.env.PACKAGES_TEST_SECOND_ADMIN_TOKEN;
const browser = await chromium.launch({ executablePath: process.env.PACKAGES_CHROMIUM_EXECUTABLE });
try {
  const context = await browser.newContext({
    viewport: { width: 1440, height: 1000 },
  });
  const page = await context.newPage();
  page.setDefaultTimeout(8000);
  await page.goto(origin);
  await page.locator("#login").waitFor({ state: "visible" });
  await page.locator("#login input").fill(owner);
  await page.locator("#login button").click();
  await page.getByRole("button", { name: "Pickups", exact: true }).click();
  await page
    .getByRole("button", { name: "Schedule pickup", exact: true })
    .click();
  await page.getByLabel("Prepaid tracking numbers").fill("TESTPICKUP123");
  await page
    .locator('#pickup-schedule-form select[name="carrier"]')
    .selectOption("ups");
  await page
    .getByRole("button", { name: "Review pickup", exact: true })
    .click();
  await expect(page.locator("#pickup-review-details")).toContainText(
    "123 Example Street",
  );
  const headers = {
    Authorization: `Bearer ${secondAdmin}`,
    "X-Packages-Actor": "ha:admin-two",
    "X-Packages-Request": "1",
    "Content-Type": "application/json",
  };
  const read = await fetch(new URL("/v1/pickups/locations", origin), {
    headers,
  });
  expect(read.status).toBe(200);
  const [site] = await read.json();
  const update = await fetch(
    new URL(`/v1/pickups/locations/${site.id}`, origin),
    {
      method: "PUT",
      headers: { ...headers, "If-Match": `"${site.revision}"` },
      body: JSON.stringify({
        location: {
          ...site.details,
          address: { ...site.details.address, street: "124 Example Street" },
        },
        make_default: true,
      }),
    },
  );
  expect(update.status).toBe(200);
  expect((await update.json()).revision).toBe(site.revision + 1);
  await page
    .getByRole("checkbox", { name: "Accept whatever pickup fee", exact: false })
    .check();
  await page.locator("#pickup-schedule-commit").click();
  await expect(page.locator("#pickup-schedule-error")).toContainText("changed");
  await expect(page.locator("#pickup-review-button")).toBeVisible();
  await expect(page.locator("#pickup-schedule-commit")).toBeHidden();
  await expect(page.getByLabel("Prepaid tracking numbers")).toHaveValue(
    "TESTPICKUP123",
  );
  await page
    .getByRole("button", { name: "Review pickup", exact: true })
    .click();
  await expect(page.locator("#pickup-review-details")).toContainText(
    "124 Example Street",
  );
  // A fresh review is available, but no new explicit scheduling action occurs.
  await context.close();
} finally {
  await browser.close();
}
