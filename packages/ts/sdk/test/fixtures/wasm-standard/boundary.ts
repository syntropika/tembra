// Acceptance-only probes of the guest's standard-import boundary. Not an example of authoring an
// Orb.
//
// The old interpreter carried WASI calls as raw binary packets through a guest-visible seam, so
// these probes forged packets with arbitrary handle numbers. A component has no such seam:
// standard imports are canonical WIT calls, and resource handles live in the component's own
// canonical handle table, so a guest can name neither a raw handle number nor another Orb's
// handle. The probes now assert that the old seams are absent, that a disposed descriptor is
// refused by the runtime, and that only descriptors this Orb retained can be inspected.
import { orb } from "@syntropika/tembra-sdk";
import { getDirectories } from "wasi:filesystem/preopens@0.3.0";
import type { Descriptor } from "wasi:filesystem/types@0.3.0";

// Descriptors deliberately retained across calls, addressed by their index in this Orb.
const retained: Descriptor[] = [];

function root(): Descriptor {
  return getDirectories()[0]![0];
}

/** Report whether an operation was accepted or the message it was refused with. */
async function outcome(operation: () => Promise<unknown>): Promise<string> {
  try {
    await operation();
    return "accepted";
  } catch (error) {
    return String((error as Error).message);
  }
}

export default orb({
  name: "boundary",
  apiVersion: 1,
  services: {
    probes: {
      methods: {
        async descriptor() {
          retained.push(root());
          return retained.length - 1;
        },
        async inspect(_environment: unknown, _context: unknown, index: number) {
          const descriptor = retained[index];
          if (!descriptor) return "unknown descriptor";
          return outcome(() => descriptor.stat());
        },
        async probes() {
          using directory = root();
          const temporary = await directory.openAt({}, "bytes.bin", {}, { read: true });
          temporary[Symbol.dispose]();
          const global = globalThis as Record<string, unknown>;
          return JSON.stringify({
            seams: ["__hostAsync", "__hostSync", "__tembraStandard", "__tembraOrb"].filter(
              (name) => name in global,
            ),
            droppedObject: await outcome(() => temporary.stat()),
          });
        },
      },
    },
  },
});
