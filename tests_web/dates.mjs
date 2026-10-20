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
