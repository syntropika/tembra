import { describe, expect, test } from "bun:test";

import {
  checkObservationLimit,
  drainFinalObservations,
  parseObservationBatch,
} from "../src/host/observability.js";
import type { ObservationBatch } from "../src/host/observability.js";

describe("bounded observation consumers", () => {
  test("refuses invalid limits before calling a runtime ABI", () => {
    for (const limit of [-1, 0.5, Number.NaN, Number.POSITIVE_INFINITY, 4_294_967_296]) {
      expect(() => {
        checkObservationLimit(limit);
      }).toThrow(RangeError);
    }
    checkObservationLimit(0);
    checkObservationLimit(4_294_967_295);
  });
  test("a disposed runtime's plain final snapshot drains once in order", () => {
    const event = {
      id: "1",
      parent: null,
      host: "1",
      root: null,
      object: null,
      claim: null,
      code_version: null,
      operation: "root",
      label: "root",
      timestamp_ns: "0",
      outcome: null,
    } as const;
    const batch: ObservationBatch = {
      events: [event, { ...event, id: "2" }],
      statistics: {
        started: 2,
        completed: 0,
        abandoned: 0,
        sampled_out: 0,
        dropped_events: 3,
        identifier_exhaustion: 0,
        clock_unavailable: 0,
        queued: 2,
        capacity: 16,
      },
    };
    const first = drainFinalObservations(batch, 1);
    expect(first.result?.events.map((value) => value.id)).toEqual(["1"]);
    const second = drainFinalObservations(first.remaining, 4);
    expect(second.result?.events.map((value) => value.id)).toEqual(["2"]);
    expect(second.remaining?.events).toEqual([]);
    expect(second.result?.statistics.dropped_events).toBe(3);
    expect(drainFinalObservations(null, 1)).toEqual({ result: null, remaining: null });
  });
});

test("ABI decoding rejects malformed correlation metadata", () => {
  expect(parseObservationBatch("null")).toBeNull();
  expect(() => parseObservationBatch('{"events":[],"statistics":{}}')).toThrow(TypeError);
  expect(() => parseObservationBatch('{"events":[{"id":1}],"statistics":{"capacity":2}}')).toThrow(
    TypeError,
  );
});
