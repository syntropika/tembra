// Acceptance-only probes: forge raw standard packets the generated bindings never produce and
// report how the host refuses them. Not an example of authoring an Orb.
import { orb } from "@syntropika/tembra-sdk";
import { getDirectories } from "wasi:filesystem/preopens@0.3.0";
import type { Descriptor } from "wasi:filesystem/types@0.3.0";
import { METHODS } from "../../../src/interpreter/standard.ts";

const host = globalThis as unknown as { __hostAsync(packet: Uint8Array): Promise<Uint8Array> };
const method = (name: string) => METHODS.findIndex((entry) => entry.name === name);
// Raw numeric probes deliberately retain their owners across calls; they must not depend on GC.
const retained: unknown[] = [];

/** Capture the first binary packet `operation` sends through the asynchronous host seam. */
async function captured(operation: () => Promise<unknown>): Promise<DataView> {
  const original = host.__hostAsync;
  let packet: Uint8Array | undefined;
  host.__hostAsync = (bytes) => {
    packet ??= bytes.slice();
    return original(bytes);
  };
  try {
    await operation().catch(() => {});
  } finally {
    host.__hostAsync = original;
  }
  return new DataView(packet!.buffer);
}

/** Send a forged CALL of `name` with one handle argument and report the outcome. */
async function forged(name: string, handle: number): Promise<string> {
  const packet = new DataView(new ArrayBuffer(14));
  packet.setUint8(0, 0);
  packet.setUint8(1, 0);
  packet.setUint32(2, method(name), true);
  packet.setBigUint64(6, BigInt(handle), true);
  try {
    await host.__hostAsync(new Uint8Array(packet.buffer));
    return "accepted";
  } catch (error) {
    return String((error as Error).message);
  }
}

function root(): Descriptor {
  return getDirectories()[0]![0];
}

/** The raw handle number the binding sends for `descriptor`. */
async function number(descriptor: Descriptor): Promise<number> {
  return Number((await captured(() => descriptor.stat())).getBigUint64(6, true));
}

export default orb({
  name: "boundary",
  apiVersion: 1,
  services: {
    probes: {
      methods: {
        async descriptor() {
          const descriptor = root();
          retained.push(descriptor);
          return number(descriptor);
        },
        async inspect(_environment: unknown, _context: unknown, handle: number) {
          return forged("stat", handle);
        },
        async probes() {
          using directory = root();
          const file = await directory.openAt({}, "bytes.bin", {}, { read: true });
          const [stream] = file.readViaStream(0n);
          retained.push(file, stream);
          const streamHandle = Number(
            (
              await captured(() => stream[Symbol.asyncIterator]().next() as Promise<unknown>)
            ).getBigUint64(2, true),
          );
          const temporary = await directory.openAt({}, "bytes.bin", {}, { read: true });
          const droppedHandle = await number(temporary);
          temporary[Symbol.dispose]();
          let droppedObject = "accepted";
          try {
            await temporary.stat();
          } catch (error) {
            droppedObject = String((error as Error).message);
          }
          return JSON.stringify({
            modules: Object.keys(
              (globalThis as unknown as { __tembraStandard: object }).__tembraStandard,
            ).sort(),
            forged: await forged("stat", 0x7fff_ffff),
            wrongType: await forged("stat", streamHandle),
            dropped: await forged("stat", droppedHandle),
            droppedObject,
            unimported: await forged("waitFor", 1),
          });
        },
      },
    },
  },
});
