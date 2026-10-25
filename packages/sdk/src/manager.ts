import { Errors } from "./errors.ts";

import type { Host } from "./host.ts";
import { TembraError } from "./error.ts";

/** Explicit infrastructure lifecycle. Replacing a host creates new memory and references. */
export class HostManager {
  #hosts = new Map<string, Host>();
  #pending = new Map<string, Promise<Host>>();
  get(name: string): Host | undefined {
    return this.#hosts.get(name);
  }
  start(name: string, create: () => Promise<Host> | Host): Promise<Host> {
    const existing = this.#hosts.get(name);
    if (existing) return Promise.resolve(existing);
    const pending = this.#pending.get(name);
    if (pending) return pending;
    const operation = Promise.resolve()
      .then(create)
      .then((host) => {
        this.#hosts.set(name, host);
        return host;
      })
      .finally(() => {
        this.#pending.delete(name);
      });
    this.#pending.set(name, operation);
    return operation;
  }
  async stop(name: string): Promise<void> {
    await this.#pending.get(name);
    const host = this.#hosts.get(name);
    if (!host) return;
    this.#hosts.delete(name);
    await host.close();
  }
  async replace(name: string, create: () => Promise<Host> | Host): Promise<Host> {
    await this.stop(name);
    return this.start(name, create);
  }
  async close(): Promise<void> {
    const names = new Set([...this.#hosts.keys(), ...this.#pending.keys()]);
    const results = await Promise.allSettled([...names].map((name) => this.stop(name)));
    const errors = results
      .filter((result) => result.status === "rejected")
      .map((result) => result.reason);
    if (errors.length) throw new AggregateError(errors, "Host cleanup failed");
  }
}

export function router<
  H extends { get(service: string, key: string | readonly string[], options?: any): any },
>(hosts: ReadonlyMap<string, H>, route: (service: string, key: readonly string[]) => string) {
  return {
    get(service: string, key: string | readonly string[], options?: any): ReturnType<H["get"]> {
      const name = route(service, typeof key === "string" ? [key] : key);
      const host = hosts.get(name);
      if (!host) throw new TembraError(Errors.Unavailable.Route, name);
      return host.get(service, key, options);
    },
  };
}
