import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import test from 'node:test';

test('date-only labels keep the carrier day in LA, including DST boundaries', () => {
  const script = `
    import assert from 'node:assert/strict';
    import { calendarDate } from './web/dates.js';
    for (const [day, next] of [['2026-10-20', '2026-10-21'], ['2026-03-07', '2026-03-08'], ['2026-10-31', '2026-11-01']]) {
      const now = new Date(day + 'T16:00:00');
      assert.equal(calendarDate(next, now), 'tomorrow');
      assert.equal(calendarDate(day, now), 'today');
    }
    const now = new Date('2026-10-20T16:00:00-07:00');
    assert.equal(calendarDate('2026-10-21T00:00:00Z', now), 'today');
    assert.equal(calendarDate('2026-10-21T00:00:00-07:00', now), 'tomorrow');
    assert.equal(calendarDate('not-a-date', now), 'date not reported');
  `;
  const result = spawnSync(process.execPath, ['--input-type=module', '-e', script], {
    cwd: new URL('..', import.meta.url), env: { ...process.env, TZ: 'America/Los_Angeles' }, encoding: 'utf8',
  });
  assert.equal(result.status, 0, result.stderr);
});

test('HA timezone overrides the remote browser and rejects missing/invalid dates', async () => {
  const { calendarDate } = await import('../web/dates.js');
  const now = new Date('2026-10-28T12:00:00Z');
  assert.equal(calendarDate('2026-10-29T03:00:00Z', now, 'America/Los_Angeles'), 'today');
  assert.equal(calendarDate('2026-10-28', now, 'America/Los_Angeles'), 'today');
  assert.equal(calendarDate('2026-10-28T00:00:00Z', now, 'America/Los_Angeles'), 'yesterday');
  assert.equal(calendarDate('2026-02-30', now), 'date not reported');
  assert.equal(calendarDate(null, now), 'date not reported');
  assert.equal(calendarDate('2026-10-28T00:00:00', now), 'date not reported');
});

test('backend calendar dates render without interpreting Amazon wording', async () => {
  const { deliveryDescription } = await import('../web/dates.js');
  const now = new Date('2026-10-31T20:00:00Z');
  const shipment = { status: 'delivered', carrier_tracking: { delivery_date: {
    kind: 'delivered', date: '2026-10-25', observed_at: '2026-10-31T19:00:00Z', timezone: 'America/Los_Angeles',
  } } };
  const actual = deliveryDescription(shipment, now, 'America/Los_Angeles');
  assert.equal(actual.label, 'Delivered');
  assert.match(actual.value, /Oct.*25/);
  assert.equal(shipment.carrier_tracking.delivered_at, undefined);
  shipment.status = 'in_transit';
  shipment.carrier_tracking.delivery_date = { kind: 'estimated', date: '2026-11-01' };
  assert.equal(deliveryDescription(shipment, now, 'America/Los_Angeles').summary, 'Expected tomorrow');
  assert.equal(deliveryDescription(shipment, new Date('2026-11-01T20:00:00Z'), 'America/Los_Angeles').summary, 'Expected today');
  shipment.carrier_tracking.delivery_date = { kind: 'estimated_range', start: '2026-11-01', end: '2026-11-02' };
  assert.match(deliveryDescription(shipment, now, 'America/Los_Angeles').summary, /^Expected tomorrow – /);
  shipment.carrier_tracking.delivery_date.end = '2026-10-30';
  assert.equal(deliveryDescription(shipment, now, 'America/Los_Angeles').summary, 'Delivery date not reported yet');
  shipment.carrier_tracking.delivery_date = { kind: 'delivered', date: '2026-11-01' };
  assert.equal(deliveryDescription(shipment, now, 'America/Los_Angeles').summary, 'Delivery date not reported yet');
});

test('actual carrier dates precede manual completion, which remains the final fallback', async () => {
  const { deliveryDescription } = await import('../web/dates.js');
  const now = new Date('2026-10-31T20:00:00Z');
  const shipment = { status: 'delivered', carrier_tracking: {
    delivered_at: '2026-10-31T01:00:00Z',
    delivery_date: { kind: 'delivered', date: '2026-10-25' },
  } };
  assert.equal(deliveryDescription(shipment, now, 'America/Los_Angeles').summary, 'Delivered yesterday');
  shipment.carrier_tracking.delivered_at = null;
  shipment.status_override = { status: 'delivered', set_at: '2026-10-31T19:00:00Z' };
  assert.match(deliveryDescription(shipment, now, 'America/Los_Angeles').summary, /Oct.*25/);
  shipment.carrier_tracking.delivery_date = null;
  assert.equal(deliveryDescription(shipment, now, 'America/Los_Angeles').summary, 'Delivered today');
  shipment.status = 'return_to_sender';
  shipment.status_override = null;
  shipment.carrier_tracking.delivered_at = '2026-10-31T01:00:00Z';
  assert.equal(deliveryDescription(shipment, now, 'America/Los_Angeles', true).summary, 'Delivered yesterday');
});

test('explicit estimated windows show their reported date bounds', async () => {
  const { deliveryDescription } = await import('../web/dates.js');
  const now = new Date('2026-10-31T20:00:00Z');
  const shipment = { status: 'in_transit', carrier_tracking: { estimated_delivery_window: { start: '2026-11-01', end: '2026-11-02' } } };
  const label = deliveryDescription(shipment, now, 'America/Los_Angeles');
  assert.match(label.summary, /^Expected tomorrow – /);
  shipment.carrier_tracking.estimated_delivery_window.end = '2026-10-30';
  assert.equal(deliveryDescription(shipment, now, 'America/Los_Angeles').summary, 'Delivery date not reported yet');
});
