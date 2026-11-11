import { orb } from "@syntropika/tembra-sdk";
import { now, waitFor } from "wasi:clocks/monotonic-clock@0.3.0";
import { getDirectories } from "wasi:filesystem/preopens@0.3.0";
import type { Descriptor } from "wasi:filesystem/types@0.3.0";

/** The single granted mount; every call returns a fresh owned handle. */
function mount(): Descriptor {
  const directories = getDirectories();
  for (const [directory] of directories.slice(1)) directory[Symbol.dispose]();
  const [first] = directories;
  if (!first) throw new Error("no granted mount");
  return first[0];
}

/** Owned by the Store across callbacks; cancellation of another callback keeps it. */
let retained: Descriptor | undefined;

export default orb({
  name: "standard",
  apiVersion: 1,
  services: {
    checks: {
      methods: {
        async open() {
          using directory = mount();
          retained = await directory.openAt({}, "bytes", {}, { read: true });
          return (await retained.stat()).size.toString();
        },
        async size() {
          if (!retained) throw new Error("open first");
          return (await retained.stat()).size.toString();
        },
        async hold() {
          const minute = 60_000_000_000n;
          const started = now();
          console.log("standard:waiting");
          try {
            await waitFor(minute);
          } catch (error) {
            throw new Error(`wait revoked: ${(error as Error).message}`);
          }
          // A revoked wait may also end early; only the full duration counts as finished.
          if (now() - started < minute) throw new Error("wait revoked before its duration");
          return "finished";
        },
        async abandon() {
          // The calls borrow the directory, so its handle stays with the Store.
          const directory = mount();
          const tick = waitFor(0n);
          for (let index = 0; index < 127; index++) directory.stat().catch(() => {});
          await tick;
          return "abandoned";
        },
        async burst() {
          using directory = mount();
          const results = await Promise.all(Array.from({ length: 96 }, () => directory.stat()));
          return results.length.toString();
        },
      },
    },
  },
});
