/** Private capability registered by a platform Host. No runtime state crosses this boundary. */
export type RpcEndpoint = {
  receive: (frame: string) => void;
  drain: () => string[];
  ready: () => Promise<void>;
  closed: () => Promise<unknown>;
  status: () => string;
  close: () => void;
  free: () => void;
};
export type HostBridge = {
  name: string;
  attach: (peer: string, child: boolean) => RpcEndpoint;
};
const hosts = new WeakMap<object, HostBridge>();
const closing = new WeakSet<object>();
export function checkManagedHostOpen(host: object): void {
  if (closing.has(host)) {
    throw new Error("parent Host is closing");
  }
}
export function registerManagerBridge(host: object, bridge: HostBridge): void {
  hosts.set(host, bridge);
}
export function managerBridge(host: object): HostBridge {
  checkManagedHostOpen(host);
  const bridge = hosts.get(host);
  if (!bridge) {
    throw new Error("Host does not support platform RPC management");
  }
  return bridge;
}

const managers = new WeakMap<object, Set<() => Promise<void>>>();
export function ownManager(host: object, close: () => Promise<void>): void {
  checkManagedHostOpen(host);
  let entries = managers.get(host);
  if (!entries) {
    entries = new Set();
    managers.set(host, entries);
  }
  entries.add(close);
}
export async function closeManagedHosts(host: object): Promise<void> {
  closing.add(host);
  const entries = managers.get(host);
  if (!entries) {
    return;
  }
  const results = await Promise.allSettled(
    [...entries].map(async (close) => {
      await close();
    }),
  );
  managers.delete(host);
  const failed = results.find((result) => result.status === "rejected");
  if (failed?.status === "rejected") {
    throw failed.reason;
  }
}

const links = new WeakMap<object, Set<() => void>>();
/** Releases observers and issued endpoint wrappers after core cleanup, before binding teardown. */
export function ownManagerLink(host: object, detach: () => void): () => void {
  let entries = links.get(host);
  if (!entries) {
    entries = new Set();
    links.set(host, entries);
  }
  entries.add(detach);
  return () => {
    entries.delete(detach);
    if (entries.size === 0) {
      links.delete(host);
    }
  };
}
export function disposeManagerLinks(host: object): void {
  const entries = links.get(host);
  links.delete(host);
  for (const detach of entries ?? []) {
    detach();
  }
}
