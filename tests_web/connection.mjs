import assert from "node:assert/strict";
import { createServer } from "node:http";
import { setImmediate } from "node:timers/promises";
import test from "node:test";
import { waitForConnection } from "../web/connection.js";

async function backend(t, handler) {
  const server = createServer(handler);
  await new Promise(resolve => server.listen(0, "127.0.0.1", resolve));
  t.after(() => {
    server.closeAllConnections();
    return new Promise(resolve => server.close(resolve));
  });
  return `http://127.0.0.1:${server.address().port}`;
}

async function read(url, signal) {
  const response = await fetch(url, { signal });
  if (!response.ok) {
    const error = new Error(`HTTP ${response.status}`);
    error.status = response.status;
    throw error;
  }
  return response.json();
}

test("HA waits through authentication, restart and non-JSON failures then connects", async t => {
  let requests = 0;
  const url = await backend(t, (_request, response) => {
    requests += 1;
    if (requests === 1) response.writeHead(401).end();
    else if (requests === 2) response.writeHead(503).end();
    else if (requests === 3) response.end("<html>Starting Home Assistant</html>");
    else response.end('{"connected":true}');
  });
  const waiting = [];
  const result = await waitForConnection(signal => read(url, signal), {
    homeAssistant: true,
    signal: new AbortController().signal,
    onWaiting: error => waiting.push(error.status ?? error.name),
  });
  assert.deepEqual(waiting, [401, 503, "SyntaxError"]);
  assert.deepEqual(result, { connected: true });
  assert.equal(requests, 4);
});

test("standalone authentication rejection returns for manual token entry", async t => {
  let requests = 0;
  const url = await backend(t, (_request, response) => {
    requests += 1;
    response.writeHead(401).end();
  });
  await assert.rejects(waitForConnection(signal => read(url, signal), {
    homeAssistant: false,
    signal: new AbortController().signal,
    onWaiting: () => assert.fail("Standalone must not enter HA pairing wait"),
  }), { status: 401 });
  assert.equal(requests, 1);
});

test("HA bounds a hung response body and recovers on the next attempt", async t => {
  let requests = 0;
  const url = await backend(t, (_request, response) => {
    requests += 1;
    response.writeHead(200, { "content-type": "application/json" });
    if (requests === 1) response.write('{"incomplete":');
    else response.end('{"connected":true}');
  });
  const waiting = [];
  const result = await waitForConnection(signal => read(url, signal), {
    homeAssistant: true,
    signal: new AbortController().signal,
    onWaiting: error => waiting.push(error.name),
  });
  assert.equal(waiting.length, 1);
  assert.deepEqual(result, { connected: true });
  assert.equal(requests, 2);
});

test("cancelling a request prevents retries and later ready state", async t => {
  const controller = new AbortController();
  let requests = 0;
  const url = await backend(t, () => {
    requests += 1;
    controller.abort();
  });
  await assert.rejects(waitForConnection(signal => read(url, signal), {
    homeAssistant: true,
    signal: controller.signal,
    onWaiting: () => assert.fail("Cancellation is not a connection failure"),
  }), { name: "AbortError" });
  assert.equal(requests, 1);
});

test("cancelling backoff stops retries and resume starts immediately", async t => {
  const controller = new AbortController();
  let requests = 0;
  const url = await backend(t, (_request, response) => {
    requests += 1;
    if (requests === 1) response.writeHead(503).end();
    else response.end('{"connected":true}');
  });
  await assert.rejects(waitForConnection(signal => read(url, signal), {
    homeAssistant: true,
    signal: controller.signal,
    onWaiting: () => controller.abort(),
  }), { name: "AbortError" });
  assert.equal(requests, 1);
  assert.deepEqual(await waitForConnection(signal => read(url, signal), {
    homeAssistant: true,
    signal: new AbortController().signal,
    onWaiting: () => assert.fail("Resume should use the now-healthy backend"),
  }), { connected: true });
  assert.equal(requests, 2);
});

test("retry backoff grows to fifteen seconds and remains capped", async t => {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  let attempts = 0;
  const result = waitForConnection(async () => {
    attempts += 1;
    if (attempts < 8) throw new Error("Starting");
    return "ready";
  }, {
    homeAssistant: true,
    signal: new AbortController().signal,
    onWaiting() {},
  });
  await setImmediate();
  for (const delay of [1_000, 2_000, 4_000, 8_000, 15_000, 15_000, 15_000]) {
    const before = attempts;
    t.mock.timers.tick(delay - 1);
    await setImmediate();
    assert.equal(attempts, before);
    t.mock.timers.tick(1);
    await setImmediate();
    assert.equal(attempts, before + 1);
  }
  assert.equal(await result, "ready");
});

test("an already-cancelled startup makes no requests", async () => {
  await assert.rejects(waitForConnection(() => assert.fail("Cancelled startup"), {
    homeAssistant: true,
    signal: AbortSignal.abort(),
    onWaiting: () => assert.fail("Cancelled startup"),
  }), { name: "AbortError" });
});
