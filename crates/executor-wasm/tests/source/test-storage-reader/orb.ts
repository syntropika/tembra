import { orb, readText, writeText } from "@syntropika/tembra-sdk";
import { getDirectories } from "wasi:filesystem/preopens@0.3.0";
import type { Descriptor } from "wasi:filesystem/types@0.3.0";

/** The granted mount containing `path`, and the path relative to it. */
function mount(path: string): [Descriptor, string] {
  const directories = getDirectories();
  const index = directories.findIndex(([, name]) => path.startsWith(`${name}/`));
  directories.forEach(([directory], other) => {
    if (other !== index) directory[Symbol.dispose]();
  });
  if (index < 0) throw new Error("outside the granted mount");
  const [directory, name] = directories[index]!;
  return [directory, path.slice(name.length + 1)];
}

/** Run a filesystem operation; a refusal becomes the `result` error string. */
async function refused<T>(operation: () => Promise<T>): Promise<T> {
  try {
    return await operation();
  } catch (error) {
    const tag = (error as { payload?: { tag?: unknown } }).payload?.tag;
    throw new Error(typeof tag === "string" ? tag : (error as Error).message);
  }
}

// The writer and reader fixtures are the same code under two Orb names, installed with different
// grants.
export default orb({
  name: "reader",
  apiVersion: 1,
  services: {
    files: {
      methods: {
        read(_environment: unknown, _context: unknown, path: string) {
          return refused(async () => {
            const [root, relative] = mount(path);
            using directory = root;
            using file = await directory.openAt({}, relative, {}, { read: true });
            return await readText(file);
          });
        },
        write(_environment: unknown, _context: unknown, path: string, content: string) {
          return refused(async () => {
            const [root, relative] = mount(path);
            using directory = root;
            using file = await directory.openAt(
              {},
              relative,
              { create: true, truncate: true },
              { write: true },
            );
            await writeText(file, content);
          });
        },
      },
    },
  },
});
