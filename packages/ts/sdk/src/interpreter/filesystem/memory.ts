import { type FilesystemProvider, fail } from "./contract.ts";
import { type Store, provider } from "./scope.ts";

// Memory storage: real directory nodes and file nodes with growable capacity.

const MEMORY_LIMIT = 2 ** 31 - 1;

class MemoryFile {
  readonly type = "file";
  data: Uint8Array = new Uint8Array(0);
  size = 0;
  modified = Date.now();
}

class MemoryDirectory {
  readonly type = "directory";
  readonly children = new Map<string, MemoryNode>();
  modified = Date.now();
  /** Removed directories accept no new entries; open descriptors keep the node. */
  removed = false;
  /** Set while attached; used to reject moving a directory into its own subtree. */
  constructor(public parent: MemoryDirectory | undefined) {}
}

type MemoryNode = MemoryFile | MemoryDirectory;

function allocate(length: number): Uint8Array {
  try {
    return new Uint8Array(length);
  } catch {
    return fail("insufficient-memory", "Memory file allocation failed");
  }
}

/** Grow capacity geometrically so repeated appends copy amortized O(size) bytes. */
function reserve(file: MemoryFile, end: number): void {
  if (end <= file.data.length) return;
  const next = allocate(Math.max(end, Math.min(MEMORY_LIMIT, file.data.length * 2 || 64)));
  next.set(file.data.subarray(0, file.size));
  file.data = next;
}

function child(parent: MemoryDirectory, name: string): MemoryNode {
  return parent.children.get(name) ?? fail("no-entry", "No such file or directory");
}

function attach(parent: MemoryDirectory, name: string, node: MemoryNode): void {
  if (parent.removed) fail("no-entry", "Directory was removed");
  parent.children.set(name, node);
  if (node.type === "directory") node.parent = parent;
  parent.modified = Date.now();
}

function detach(parent: MemoryDirectory, name: string, node: MemoryNode): void {
  parent.children.delete(name);
  parent.modified = Date.now();
  if (node.type === "directory") {
    node.parent = undefined;
    node.removed = true;
  }
}

function memoryStore(root: MemoryDirectory): Store<MemoryNode> {
  const directory = (node: MemoryNode) => node as MemoryDirectory;
  const file = (node: MemoryNode) => node as MemoryFile;
  // Each operation completes synchronously, so memory mutations are atomic.
  return {
    root,
    limit: MEMORY_LIMIT,
    type: (node) => node.type,
    lookup: async (parent, name) => child(directory(parent), name),
    async create(parent, name, type) {
      const into = directory(parent);
      if (into.children.has(name)) fail("exist", "Entry already exists");
      const node = type === "file" ? new MemoryFile() : new MemoryDirectory(into);
      attach(into, name, node);
      return node;
    },
    retain: async (node) => node,
    release() {},
    async stat(node) {
      return node.type === "file"
        ? { type: "file", size: BigInt(node.size), modified: node.modified }
        : { type: "directory", size: 0n, modified: node.modified };
    },
    same: async (a, b) => a === b,
    // Iterates the live map lazily; entries added or removed meanwhile follow Map semantics.
    async *entries(node) {
      for (const [name, entry] of directory(node).children) yield { name, type: entry.type };
    },
    async read(node, offset, length) {
      const target = file(node);
      if (offset >= target.size) return new Uint8Array(0);
      return target.data.slice(offset, Math.min(target.size, offset + length));
    },
    async write(node, offset, data) {
      const target = file(node);
      const start = offset ?? target.size;
      const end = start + data.byteLength;
      if (end > MEMORY_LIMIT) fail("file-too-large", "Memory file limit exceeded");
      reserve(target, end);
      // Bytes between the old size and `start` are already zero; truncation zero-fills.
      target.data.set(data, start);
      target.size = Math.max(target.size, end);
      target.modified = Date.now();
    },
    async resize(node, size) {
      const target = file(node);
      if (size < target.size) {
        if (size < target.data.length / 4) {
          const next = allocate(size);
          next.set(target.data.subarray(0, size));
          target.data = next;
        } else target.data.fill(0, size, target.size);
      } else reserve(target, size);
      target.size = size;
      target.modified = Date.now();
    },
    async sync() {},
    async remove(parent, name, type) {
      const from = directory(parent);
      const node = child(from, name);
      if (node.type !== type)
        fail(type === "file" ? "is-directory" : "not-directory", "Entry has another type");
      if (node.type === "directory" && node.children.size)
        fail("not-empty", "Directory is not empty");
      detach(from, name, node);
    },
    async rename(fromNode, fromName, toNode, toName) {
      const from = directory(fromNode);
      const to = directory(toNode);
      const node = child(from, fromName);
      if (to.removed) fail("no-entry", "Directory was removed");
      const replaced = to.children.get(toName);
      if (replaced === node) return;
      if (node.type === "directory")
        for (let ancestor: MemoryDirectory | undefined = to; ancestor; ancestor = ancestor.parent)
          if (ancestor === node) fail("invalid", "Cannot move a directory into itself");
      if (replaced) {
        if (node.type === "file" && replaced.type === "directory")
          fail("is-directory", "Target is a directory");
        if (node.type === "directory" && replaced.type === "file")
          fail("not-directory", "Target is not a directory");
        if (replaced.type === "directory" && replaced.children.size)
          fail("not-empty", "Target directory is not empty");
        detach(to, toName, replaced);
      }
      detach(from, fromName, node);
      if (node.type === "directory") node.removed = false;
      attach(to, toName, node);
    },
  };
}

/** Supply shared ephemeral storage without granting any Orb access automatically. */
export function memoryFilesystem(
  files: Readonly<Record<string, string | Uint8Array>> = {},
): FilesystemProvider {
  const root = new MemoryDirectory(undefined);
  const encoder = new TextEncoder();
  for (const [path, value] of Object.entries(files)) {
    const segments = path.split("/");
    if (segments.some((part) => !part || part === "." || part === ".." || part.includes("\0")))
      throw new Error("Memory filesystem paths must be normalized relative paths");
    let parent = root;
    for (const segment of segments.slice(0, -1)) {
      let next = parent.children.get(segment);
      if (!next) attach(parent, segment, (next = new MemoryDirectory(parent)));
      if (next.type !== "directory") throw new Error("Memory path overlaps a file");
      parent = next;
    }
    const name = segments.at(-1)!;
    if (parent.children.has(name)) throw new Error("Memory path overlaps a file");
    const node = new MemoryFile();
    node.data = typeof value === "string" ? encoder.encode(value) : value.slice();
    node.size = node.data.byteLength;
    attach(parent, name, node);
  }
  return provider(memoryStore(root));
}
