import type { Grants } from "../authoring/types.ts";
import type { HttpProvider } from "./http-provider.ts";
import { type Address, isImplicit, key, parseSocketAddress } from "./socket-addresses.ts";
import type { SocketProvider } from "./socket-provider.ts";
import type { HttpClient } from "./standard-http.ts";

export interface BrowserNetworkProviders {
  http?: Readonly<Record<string, HttpProvider>>;
  network?: Readonly<Record<string, SocketProvider>>;
}

/** Address uses checked before a socket side effect, as upstream `SocketAddrUse`. */
export type SocketUse =
  | "tcp-bind"
  | "tcp-listen"
  | "tcp-accept"
  | "tcp-connect"
  | "udp-bind"
  | "udp-send"
  | "udp-receive";

/**
 * The socket and lookup authority of exactly one provider: the grants naming that provider,
 * never another provider's. Rules mirror native `Sockets::allows`.
 */
export class SocketGrants {
  readonly #connect = new Set<string>();
  readonly #listen: Address[] = [];
  readonly #listenKeys = new Set<string>();
  readonly #bind = new Set<string>();
  readonly #datagrams = new Set<string>();
  readonly resolve: boolean;

  constructor(grants: Grants, provider: string) {
    let resolve = false;
    const parse = (text: string) => {
      const address = parseSocketAddress(text);
      if (!address) throw new Error(`Invalid network grant address ${text}`);
      return address;
    };
    for (const grant of grants.network ?? []) {
      if (grant.provider !== provider) continue;
      for (const text of grant.connect ?? []) this.#connect.add(key(parse(text)));
      for (const text of grant.listen ?? []) {
        const address = parse(text);
        this.#listen.push(address);
        this.#listenKeys.add(key(address));
      }
      for (const text of grant.bind ?? []) this.#bind.add(key(parse(text)));
      for (const text of grant.datagrams ?? []) this.#datagrams.add(key(parse(text)));
      resolve ||= grant.resolve === true;
    }
    this.resolve = resolve;
  }

  /** Any TCP authority (`allow_tcp`). */
  get tcp(): boolean {
    return this.#connect.size > 0 || this.#listen.length > 0 || this.#bind.size > 0;
  }

  /** Any UDP authority (`allow_udp`). */
  get udp(): boolean {
    return this.#datagrams.size > 0 || this.#bind.size > 0;
  }

  allows(address: Address, use: SocketUse): boolean {
    const exact = key(address);
    switch (use) {
      case "tcp-connect":
        return this.#connect.has(exact);
      case "tcp-bind":
        return (
          (isImplicit(address) && this.tcp) || this.#bind.has(exact) || this.#listenKeys.has(exact)
        );
      case "tcp-listen":
        // Rust compares `IpAddr` here, so flow information and scope do not participate.
        return this.#listen.some(
          (granted) =>
            granted.family === address.family &&
            granted.ip.every((part, index) => part === address.ip[index]) &&
            (granted.port === 0 || granted.port === address.port),
        );
      case "tcp-accept":
        return this.#listen.length > 0;
      case "udp-bind":
        return (isImplicit(address) && this.#datagrams.size > 0) || this.#bind.has(exact);
      case "udp-send":
      case "udp-receive":
        return this.#datagrams.has(exact);
    }
  }
}

/** The provider and authority behind one Orb's `wasi:sockets` import. */
export interface SocketAccess {
  /** `undefined` when the bound provider name has no registered socket provider. */
  readonly provider: SocketProvider | undefined;
  readonly grants: SocketGrants;
}

/**
 * Granted network selections of one Orb:
 * - HTTP is served only through standard `wasi:http` (`provider`, checked by the standard host
 *   before any provider call);
 * - sockets and name lookup use the providers bound to the Orb's exact WIT imports (`sockets`).
 */
export class BrowserNetwork implements HttpClient {
  #disposed = false;

  constructor(
    readonly grants: Grants,
    readonly providers: BrowserNetworkProviders,
  ) {
    for (const grant of grants.http ?? []) {
      if (!grant.provider.trim() || !grant.origins.length)
        throw new Error("HTTP grants require a provider and at least one origin");
      for (const origin of grant.origins) {
        const url = new URL(origin);
        if (
          !["http:", "https:"].includes(url.protocol) ||
          !url.hostname ||
          url.username ||
          url.password ||
          url.href.includes("#") ||
          url.pathname !== "/" ||
          url.href.includes("?") ||
          url.hostname.includes("*") ||
          url.port === "0"
        )
          throw new Error(
            "HTTP grants require exact http(s) origins without credentials, paths, queries, wildcards or port 0",
          );
      }
    }
    // Native checks these addresses at installation too; refuse a malformed grant early here.
    for (const grant of grants.network ?? []) new SocketGrants(grants, grant.provider);
  }

  /**
   * The provider granted for exactly `url`'s origin. Standard `wasi:http` sends call this before
   * any provider work; HTTP grants never follow raw socket grants or the reverse.
   */
  provider(url: URL): HttpProvider | undefined {
    if (
      this.#disposed ||
      (url.protocol !== "http:" && url.protocol !== "https:") ||
      url.username ||
      url.password
    )
      return undefined;
    const grant = this.grants.http?.find((grant) =>
      grant.origins.some((origin) => new URL(origin).origin === url.origin),
    );
    return grant && this.providers.http?.[grant.provider];
  }

  /** The socket provider and authority for the provider name an import binding selected. */
  sockets(provider: string | undefined): SocketAccess | undefined {
    if (provider === undefined) return undefined;
    return {
      provider: this.providers.network?.[provider],
      grants: new SocketGrants(this.grants, provider),
    };
  }

  /** Physical sockets are owned and closed by the revision's standard host. */
  dispose(): void {
    this.#disposed = true;
  }
}
