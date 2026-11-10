import { mountAppShell } from "./app-kit.js";

const header = document.querySelector(".app-header");
const footer = document.getElementById("app-footer");
if (!(header instanceof HTMLElement) || !(footer instanceof HTMLElement)) {
  throw new Error("The app shell is unavailable.");
}
export const shell = mountAppShell({
  name: "Packages",
  header,
  footer,
  sourceUrl: "https://github.com/jessfraz/ha-packages",
  brand: [...header.querySelectorAll("#app-brand > svg")],
  actions: [...header.querySelectorAll(":scope > button")],
});
shell.version.id = "app-version";
shell.status.id = "service-status";
