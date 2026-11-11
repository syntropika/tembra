import { orb, readChunks, writeChunks } from "@syntropika/tembra-sdk";
import * as system from "wasi:clocks/system-clock@0.3.0";
import * as monotonic from "wasi:clocks/monotonic-clock@0.3.0";
import * as secure from "wasi:random/random@0.3.0";
import * as insecure from "wasi:random/insecure@0.3.0";
import { getInsecureSeed } from "wasi:random/insecure-seed@0.3.0";
import * as environment from "wasi:cli/environment@0.3.0";
import { readViaStream } from "wasi:cli/stdin@0.3.0";
import { writeViaStream as stdout } from "wasi:cli/stdout@0.3.0";
import { writeViaStream as stderr } from "wasi:cli/stderr@0.3.0";
import { exitWithCode } from "wasi:cli/exit@0.3.0";
import { getTerminalStdin } from "wasi:cli/terminal-stdin@0.3.0";
import { getTerminalStdout } from "wasi:cli/terminal-stdout@0.3.0";
import { getTerminalStderr } from "wasi:cli/terminal-stderr@0.3.0";

export default orb({
  name: "system",
  apiVersion: 1,
  services: {
    scenario: {
      methods: {
        inspect() {
          const wall = system.now(),
            resolution = system.getResolution();
          const seed = getInsecureSeed();
          return JSON.stringify({
            wall: [wall.seconds.toString(), wall.nanoseconds],
            resolution: resolution.toString(),
            monotonic: monotonic.now().toString(),
            monotonicResolution: monotonic.getResolution().toString(),
            secureLength: secure.getRandomBytes(16n).length,
            secureU64: secure.getRandomU64().toString(),
            insecureLength: insecure.getInsecureRandomBytes(17n).length,
            insecureU64: insecure.getInsecureRandomU64().toString(),
            seed: seed.map(String),
            seedStable: seed.every((word, index) => word === getInsecureSeed()[index]),
            date: Date.now(),
            random: Math.random(),
            arguments: environment.getArguments(),
            environment: environment.getEnvironment(),
            cwd: environment.getInitialCwd() ?? null,
            terminal: [getTerminalStdin(), getTerminalStdout(), getTerminalStderr()].every(
              (value) => value === undefined,
            ),
          });
        },
        async transfer() {
          const [stream, done] = readViaStream();
          let length = 0,
            first = -1,
            last = -1;
          const output = writeChunks(
            (async function* () {
              for await (const bytes of readChunks(stream)) {
                if (first === -1 && bytes.length) first = bytes[0]!;
                if (bytes.length) last = bytes[bytes.length - 1]!;
                length += bytes.length;
                yield bytes;
              }
            })(),
          );
          // Lifted WIT futures are handles: `read()` returns their value once.
          const result = await stdout(output).read();
          const read = await done.read();
          const error = await stderr(
            writeChunks(
              (async function* () {
                yield new Uint8Array([0, 255, 128, 10]);
              })(),
            ),
          ).read();
          if (result.tag !== "ok" || read.tag !== "ok" || error.tag !== "ok")
            throw new Error("standard stdio failed");
          return JSON.stringify({ length, first, last });
        },
        async wait(_self: unknown, _context: unknown, milliseconds: number) {
          await monotonic.waitUntil(monotonic.now() + BigInt(milliseconds) * 1_000_000n);
        },
        stop(_self: unknown, _context: unknown, code: number) {
          try {
            exitWithCode(code);
          } catch {
            return 99;
          }
          return 100;
        },
      },
    },
  },
});
