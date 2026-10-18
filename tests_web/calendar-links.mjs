import assert from "node:assert/strict";
import test from "node:test";
import { installCalendarNavigation } from "../custom_components/shipments/frontend/calendar-links.js";

const ID = "0193c69f-e5c4-7e93-b6cb-8172dacb617e";
const CALENDAR = "calendar.shipments_inbound_deliveries";

class CalendarElement extends EventTarget {
  hass = {
    states: {
      [CALENDAR]: {
        state: "off",
        attributes: {
          shipments_calendar: true,
          shipments_app_url: "/shipments/shipments/",
        },
      },
    },
  };

  open(overrides = {}) {
    const event = new CustomEvent("show-dialog", {
      bubbles: true,
      composed: true,
      detail: {
        dialogTag: "dialog-calendar-event-detail",
        dialogParams: { calendarId: CALENDAR, entry: { uid: ID } },
        ...overrides,
      },
    });
    this.dispatchEvent(event);
    return event;
  }
}

test("native calendar event opens its shipment through supervised ingress", t => {
  const calendar = new CalendarElement();
  const destinations = [];
  t.after(installCalendarNavigation(calendar, url => destinations.push(url)));
  const event = calendar.open();
  assert.deepEqual(destinations, [`/shipments/shipments/shipment%3D${ID}`]);
  assert.equal(event.cancelBubble, true);
});

test("standalone calendar opens the configured service and exact shipment hash", t => {
  const calendar = new CalendarElement();
  calendar.hass.states[CALENDAR].attributes.shipments_app_url = "https://shipments.example:8443/";
  const destinations = [];
  t.after(installCalendarNavigation(calendar, url => destinations.push(url)));
  calendar.open();
  assert.deepEqual(destinations, [`https://shipments.example:8443/#shipment=${ID}`]);
});

test("other calendars, dialogs and malformed event identities retain native behavior", t => {
  const calendar = new CalendarElement();
  t.after(installCalendarNavigation(calendar, () => assert.fail("Unexpected navigation")));
  for (const detail of [
    { dialogTag: "dialog-more-info" },
    { dialogParams: { calendarId: "calendar.other", entry: { uid: ID } } },
    { dialogParams: { calendarId: CALENDAR, entry: {} } },
    { dialogParams: { calendarId: CALENDAR, entry: { uid: "../settings" } } },
    { dialogParams: { calendarId: CALENDAR, entry: { uid: `${ID}?url=evil` } } },
  ]) assert.equal(calendar.open(detail).cancelBubble, false);
});

test("unavailable, unknown, unloaded and unrelated entities retain native behavior", t => {
  const calendar = new CalendarElement();
  t.after(installCalendarNavigation(calendar, () => assert.fail("Unexpected navigation")));
  for (const state of ["unavailable", "unknown"]) {
    calendar.hass.states[CALENDAR].state = state;
    assert.equal(calendar.open().cancelBubble, false);
  }
  calendar.hass.states[CALENDAR].state = "off";
  calendar.hass.states[CALENDAR].attributes.shipments_calendar = false;
  assert.equal(calendar.open().cancelBubble, false);
  delete calendar.hass.states[CALENDAR];
  assert.equal(calendar.open().cancelBubble, false);
});

test("unsafe or absent destination metadata cannot take over a calendar click", t => {
  const calendar = new CalendarElement();
  t.after(installCalendarNavigation(calendar, () => assert.fail("Unexpected navigation")));
  for (const base of [
    undefined, "", "//evil.example/", "javascript:alert(1)",
    "https://user:secret@example.test/", "https://example.test/other/",
    "https://example.test/?redirect=evil", "https://example.test/#other",
  ]) {
    calendar.hass.states[CALENDAR].attributes.shipments_app_url = base;
    assert.equal(calendar.open().cancelBubble, false);
  }
});

test("a module upgrade retires old listeners and unload disposes its replacement", () => {
  const calendar = new CalendarElement();
  const first = [];
  const second = [];
  const disposeFirst = installCalendarNavigation(calendar, url => first.push(url));
  calendar.open();
  const disposeSecond = installCalendarNavigation(calendar, url => second.push(url));
  disposeFirst();
  calendar.open();
  assert.equal(first.length, 1);
  assert.equal(second.length, 1);
  disposeSecond();
  assert.equal(calendar.open().cancelBubble, false);
  assert.equal(second.length, 1);
});

test("reloading a calendar restores links without registering another listener", t => {
  const calendar = new CalendarElement();
  const state = calendar.hass.states[CALENDAR];
  const destinations = [];
  t.after(installCalendarNavigation(calendar, url => destinations.push(url)));
  delete calendar.hass.states[CALENDAR];
  assert.equal(calendar.open().cancelBubble, false);
  calendar.hass.states[CALENDAR] = state;
  calendar.open();
  assert.equal(destinations.length, 1);
});
