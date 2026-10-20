import { test } from "bun:test";
import assert from "node:assert/strict";

import { disposeManagerLinks } from "../src/manager/bridge.js";
import { framePump } from "../src/manager/channel.js";
import type { ManagedChannel, ManagedMessage } from "../src/manager/channel.js";

function fixture() {
  const listeners = new Set<(message: ManagedMessage) => void>();
  const calls = { close: 0, free: 0 };
  let coreClosed = false;
  let tracked = true;
  const channel: ManagedChannel = {
    send: () => undefined,
    onMessage(listener) {
      listeners.add(listener);
      return () => {
        listeners.delete(listener);
      };
    },
    onFailure: () => () => undefined,
    terminate: async () => undefined,
  };
  const endpoint = {
    receive: () => {
      assert.ok(tracked);
    },
    drain: () => {
      assert.ok(tracked);
      return [];
    },
    ready: async () => undefined,
    closed: async () => undefined,
    status: () => "open",
    close() {
      assert.ok(!coreClosed, "core teardown can already invalidate the endpoint's close handle");
      calls.close += 1;
    },
    free() {
      assert.ok(tracked, "a binding wrapper is released exactly once");
      tracked = false;
      calls.free += 1;
    },
  };
  return {
    channel,
    endpoint,
    calls,
    listeners,
    closeCore() {
      coreClosed = true;
    },
    freeRemainingWrappers() {
      if (tracked) {
        endpoint.free();
      }
    },
  };
}

test("Host teardown releases the issued endpoint before bulk wrapper disposal", () => {
  const state = fixture();
  const host = {};
  const pump = framePump(state.endpoint, state.channel, assert.fail, true, host);
  try {
    state.closeCore();
    disposeManagerLinks(host);
    assert.deepEqual(state.calls, { close: 0, free: 1 });
    assert.equal(state.listeners.size, 0);
    state.freeRemainingWrappers();
    pump.close();
    disposeManagerLinks(host);
    assert.deepEqual(state.calls, { close: 0, free: 1 });
  } finally {
    pump.close();
  }
});

test("explicit pump close releases the endpoint once before later Host teardown", () => {
  const state = fixture();
  const host = {};
  const pump = framePump(state.endpoint, state.channel, assert.fail, true, host);
  pump.close();
  assert.deepEqual(state.calls, { close: 1, free: 1 });
  assert.equal(state.listeners.size, 0);
  state.closeCore();
  disposeManagerLinks(host);
  state.freeRemainingWrappers();
  pump.close();
  assert.deepEqual(state.calls, { close: 1, free: 1 });
});
