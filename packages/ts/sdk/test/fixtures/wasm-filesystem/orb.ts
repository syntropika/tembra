import { orb, readBytes, writeBytes, writeChunks } from "@syntropika/tembra-sdk";
import { getDirectories } from "wasi:filesystem/preopens@0.3.0";

/** Operations unavailable on a browser storage adapter report their standard error code. */
async function optional(operation: () => Promise<unknown>): Promise<string> {
  try {
    await operation();
    return "ok";
  } catch (error) {
    const code = (error as { payload?: { tag?: string } }).payload?.tag;
    if (code !== "unsupported") throw error;
    return code;
  }
}

/** The complete filesystem scenario; refusals throw the standard error. */
async function roundtrip(): Promise<string> {
  using root = getDirectories()[0]![0];
  await root.createDirectoryAt("roundtrip");
  const pattern = Uint8Array.from({ length: 251 }, (_, index) => index);
  const bytes = new Uint8Array(150_000);
  for (let offset = 0; offset < bytes.length; offset += pattern.length)
    bytes.set(pattern.subarray(0, bytes.length - offset), offset);
  let directoryRename = "nested";
  let listing: string[] = [];
  let times = "";
  let links = "";
  let symlinks = "";
  {
    using directory = await root.openAt(
      {},
      "roundtrip",
      { directory: true },
      { mutateDirectory: true },
    );
    await directory.createDirectoryAt("nested");
    {
      using nested = await directory.openAt(
        {},
        "nested",
        { directory: true },
        { mutateDirectory: true },
      );
      {
        using file = await directory.openAt(
          {},
          "data.bin",
          { create: true, exclusive: true },
          { read: true, write: true },
        );
        await writeBytes(file, bytes, { offset: 3n });
        // A lifted WIT future is a handle; `read()` returns its result.
        const appended = await file
          .appendViaStream(writeChunks([new Uint8Array([255, 128, 0])]))
          .read();
        if (appended.tag !== "ok") throw new Error("append failed");
        const read = await readBytes(file, { maxBytes: 200_000 });
        if (read.length !== bytes.length + 6 || read.subarray(0, 3).some((byte) => byte !== 0))
          throw new Error("offset write length or gap is incorrect");
        for (let index = 0; index < bytes.length; index++)
          if (read[index + 3] !== bytes[index]) throw new Error("binary roundtrip changed a byte");
        if (read[read.length - 3] !== 255 || read[read.length - 2] !== 128)
          throw new Error("append changed a byte");
        await file.setSize(9n);
        if ((await file.stat()).size !== 9n) throw new Error("truncate failed");
        const flags = await file.getFlags();
        if (!flags.read || !flags.write || (await file.getType()).tag !== "regular-file")
          throw new Error("descriptor flags or type is incorrect");
        const hash = await file.metadataHash();
        if (typeof hash.lower !== "bigint" || typeof hash.upper !== "bigint")
          throw new Error("metadata hash lost integer precision");
        await file.advise(0n, 9n, "sequential");
        await file.syncData();
        await file.sync();
        await file.setTimes({ tag: "no-change" }, { tag: "no-change" });
        times = await optional(() => file.setTimes({ tag: "no-change" }, { tag: "now" }));
        links = await optional(async () => {
          await directory.linkAt({}, "data.bin", directory, "hard.bin");
          using linked = await directory.openAt({}, "hard.bin", {}, { read: true });
          if (!(await file.isSameObject(linked))) throw new Error("hard link identity changed");
          await directory.unlinkFileAt("hard.bin");
        });
        symlinks = await optional(async () => {
          await directory.symlinkAt("data.bin", "alias");
          if ((await directory.readlinkAt("alias")) !== "data.bin")
            throw new Error("readlink changed the target");
          await directory.unlinkFileAt("alias");
        });
        await directory.renameAt("data.bin", nested, "renamed.bin");
        using reopened = await nested.openAt({}, "renamed.bin", {}, { read: true });
        if (!(await file.isSameObject(reopened)))
          throw new Error("rename changed the opened file identity");
        if ((await file.stat()).size !== 9n || (await nested.statAt({}, "renamed.bin")).size !== 9n)
          throw new Error("renamed file metadata is incorrect");
        const [entries, done] = nested.readDirectory();
        for await (const entry of entries) listing.push(entry.name);
        if ((await done.read()).tag !== "ok") throw new Error("directory stream failed");
      }
      await nested.unlinkFileAt("renamed.bin");
    }
    if ((await optional(() => directory.renameAt("nested", directory, "moved"))) === "ok")
      directoryRename = "moved";
    await directory.removeDirectoryAt(directoryRename);
  }
  await root.removeDirectoryAt("roundtrip");
  return JSON.stringify({
    length: bytes.length + 6,
    listing,
    times,
    links,
    symlinks,
    directoryRename,
  });
}

export default orb({
  name: "filesystem",
  apiVersion: 1,
  services: {
    scenario: {
      methods: {
        async roundtrip() {
          return roundtrip();
        },
        // A refused WASI call carries its `error-code` variant as `payload`; report its tag
        // through the declared failure lane.
        async roundtripChecked() {
          try {
            return await roundtrip();
          } catch (error) {
            throw (error as { payload?: { tag?: string } }).payload?.tag ?? String(error);
          }
        },
      },
    },
  },
});
