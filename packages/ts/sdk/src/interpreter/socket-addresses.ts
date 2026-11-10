/**
 * Socket addresses for the browser `wasi:sockets` host, with the same meaning as the native
 * host:
 * - equality and grant matching use Rust `SocketAddr` identity (IP, port, IPv6 flow
 *   information and scope id), never textual spelling;
 * - validity predicates mirror pinned `wasmtime-wasi` 51.0.0-rc.1 `sockets/mod.rs`;
 * - grant strings parse as Rust `SocketAddr::from_str`;
 * - name lookup input parses as `url::Host::parse`.
 */
import type { IpAddress, IpAddressFamily, IpSocketAddress } from "wasi:sockets/types@0.3.0";

/** One socket address with exact numeric fields; IPv4 has four octets, IPv6 eight segments. */
export interface Address {
  readonly family: IpAddressFamily;
  readonly ip: readonly number[];
  readonly port: number;
  readonly flowInfo: number;
  readonly scopeId: number;
}

const uint = (value: unknown, maximum: number): value is number =>
  typeof value === "number" && Number.isInteger(value) && value >= 0 && value <= maximum;

/** Read a WIT `ip-socket-address`, validating every field; `undefined` when malformed. */
export function fromWit(value: unknown): Address | undefined {
  const tag = (value as { tag?: unknown })?.tag;
  const val = (value as { val?: Record<string, unknown> })?.val;
  if (!val || (tag !== "ipv4" && tag !== "ipv6")) return undefined;
  const ip = val.address;
  const width = tag === "ipv4" ? 4 : 8;
  const maximum = tag === "ipv4" ? 0xff : 0xffff;
  if (!Array.isArray(ip) || ip.length !== width || !ip.every((part) => uint(part, maximum)))
    return undefined;
  if (!uint(val.port, 0xffff)) return undefined;
  if (tag === "ipv4")
    return { family: "ipv4", ip: [...ip], port: val.port, flowInfo: 0, scopeId: 0 };
  if (!uint(val.flowInfo, 0xffffffff) || !uint(val.scopeId, 0xffffffff)) return undefined;
  return {
    family: "ipv6",
    ip: [...ip],
    port: val.port,
    flowInfo: val.flowInfo,
    scopeId: val.scopeId,
  };
}

/** A fresh WIT value; never shares arrays with host state. */
export function toWit(address: Address): IpSocketAddress {
  return address.family === "ipv4"
    ? {
        tag: "ipv4",
        val: { port: address.port, address: [...address.ip] as [number, number, number, number] },
      }
    : {
        tag: "ipv6",
        val: {
          port: address.port,
          flowInfo: address.flowInfo,
          address: [...address.ip] as [
            number,
            number,
            number,
            number,
            number,
            number,
            number,
            number,
          ],
          scopeId: address.scopeId,
        },
      };
}

/** Rust `SocketAddr` identity: equal keys are equal addresses. */
export const key = (address: Address): string =>
  `${address.family}|${address.ip.join(".")}|${address.port}|${address.flowInfo}|${address.scopeId}`;

export const unspecified = (family: IpAddressFamily): Address => ({
  family,
  ip: new Array<number>(family === "ipv4" ? 4 : 8).fill(0),
  port: 0,
  flowInfo: 0,
  scopeId: 0,
});

const isZero = (ip: readonly number[]) => ip.every((part) => part === 0);

/** The IPv4 address an IPv6 `::ffff:a.b.c.d` maps (Rust `to_ipv4_mapped`). */
function mappedIpv4(address: Address): readonly number[] | undefined {
  const [a, b, c, d, e, f, g, h] = address.ip as number[];
  if (address.family !== "ipv6" || a || b || c || d || e || f !== 0xffff) return undefined;
  return [g! >> 8, g! & 0xff, h! >> 8, h! & 0xff];
}

/** Rust `IpAddr::to_canonical`: an IPv4-mapped IPv6 address becomes IPv4. */
function canonicalIp(address: Address): { family: IpAddressFamily; ip: readonly number[] } {
  const mapped = mappedIpv4(address);
  return mapped ? { family: "ipv4", ip: mapped } : address;
}

/** Upstream `is_valid_unicast_address`. */
export function isUnicast(address: Address): boolean {
  const { family, ip } = canonicalIp(address);
  if (family === "ipv4") return !(ip[0]! >= 224 && ip[0]! <= 239) && !ip.every((o) => o === 255);
  return ip[0]! >> 8 !== 0xff;
}

/** Upstream `is_valid_remote_address`. */
export const isRemote = (address: Address): boolean =>
  !isZero(canonicalIp(address).ip) && address.port !== 0;

/** Upstream `is_valid_address_family`, including IPv4-compatible and mapped IPv6 refusal. */
export function hasFamily(address: Address, family: IpAddressFamily): boolean {
  if (address.family !== family) return false;
  if (family === "ipv4") return true;
  const ip = address.ip;
  const compatible =
    ip.slice(0, 6).every((part) => part === 0) &&
    !isZero(ip) &&
    !(ip.slice(0, 7).every((part) => part === 0) && ip[7] === 1);
  return !compatible && mappedIpv4(address) === undefined;
}

/** An implicit bind by the OS uses the wildcard address and an ephemeral port. */
export const isImplicit = (address: Address): boolean => isZero(address.ip) && address.port === 0;

/** Strict dotted quad, as Rust `Ipv4Addr::from_str` (no leading zeros, no shorthand). */
export function parseIpv4(text: string): number[] | undefined {
  const parts = text.split(".");
  if (parts.length !== 4) return undefined;
  const octets = parts.map((part) =>
    /^(0|[1-9][0-9]{0,2})$/.test(part) ? Number(part) : Number.NaN,
  );
  return octets.every((octet) => octet <= 255) ? octets : undefined;
}

/** Rust `Ipv6Addr::from_str`: 1–4 hex digits per group, one `::` for at least one group. */
export function parseIpv6(text: string): number[] | undefined {
  const halves = text.split("::");
  if (halves.length > 2) return undefined;
  const groups = (part: string): number[] | undefined => {
    if (part === "") return [];
    const pieces = part.split(":");
    const out: number[] = [];
    for (const [index, piece] of pieces.entries()) {
      if (index === pieces.length - 1 && piece.includes(".")) {
        const v4 = parseIpv4(piece);
        if (!v4) return undefined;
        out.push((v4[0]! << 8) | v4[1]!, (v4[2]! << 8) | v4[3]!);
      } else if (/^[0-9a-fA-F]{1,4}$/.test(piece)) out.push(Number.parseInt(piece, 16));
      else return undefined;
    }
    return out;
  };
  const head = groups(halves[0]!);
  if (!head) return undefined;
  if (halves.length === 1) return head.length === 8 ? head : undefined;
  if (halves[0]!.includes(".")) return undefined;
  const tail = groups(halves[1]!);
  if (!tail || head.length + tail.length > 7) return undefined;
  return [...head, ...new Array<number>(8 - head.length - tail.length).fill(0), ...tail];
}

/** A grant string as Rust `SocketAddr::from_str`: `a.b.c.d:port` or `[v6(%scope)]:port`. */
export function parseSocketAddress(text: string): Address | undefined {
  const colon = text.lastIndexOf(":");
  const portText = text.slice(colon + 1);
  if (colon < 0 || !/^[0-9]{1,5}$/.test(portText) || Number(portText) > 0xffff) return undefined;
  const port = Number(portText);
  const host = text.slice(0, colon);
  if (host.startsWith("[") && host.endsWith("]")) {
    const [ipText, scopeText, extra] = host.slice(1, -1).split("%");
    if (extra !== undefined) return undefined;
    if (
      scopeText !== undefined &&
      !(/^[0-9]{1,10}$/.test(scopeText) && Number(scopeText) <= 0xffffffff)
    )
      return undefined;
    const ip = parseIpv6(ipText!);
    return ip && { family: "ipv6", ip, port, flowInfo: 0, scopeId: Number(scopeText ?? 0) };
  }
  const ip = parseIpv4(host);
  return ip && { family: "ipv4", ip, port, flowInfo: 0, scopeId: 0 };
}

/** WIT `ip-address` of `family` and `ip`. */
const ipAddress = (family: IpAddressFamily, ip: readonly number[]): IpAddress =>
  ({ tag: family, val: [...ip] }) as IpAddress;

/** Validate and canonicalize one provider lookup result; mapped IPv6 becomes IPv4. */
export function checkedIpAddress(value: unknown): IpAddress | undefined {
  const tag = (value as { tag?: unknown })?.tag;
  const ip = (value as { val?: unknown })?.val;
  const width = tag === "ipv4" ? 4 : tag === "ipv6" ? 8 : 0;
  const maximum = tag === "ipv4" ? 0xff : 0xffff;
  if (!width || !Array.isArray(ip) || ip.length !== width || !ip.every((p) => uint(p, maximum)))
    return undefined;
  const canonical = canonicalIp({
    family: tag as IpAddressFamily,
    ip,
    port: 0,
    flowInfo: 0,
    scopeId: 0,
  });
  return ipAddress(canonical.family, canonical.ip);
}

/**
 * WHATWG forbidden domain code points (C0 controls, space, `"#%/:<>?@[\]^|`, DEL) other than
 * `%`, which `url::Host::parse` percent-decodes first, exactly as the URL host parser does.
 */
const FORBIDDEN_ASCII = new Set('#/:<>?@[\\]^|"');
const forbidden = (text: string): boolean =>
  [...text].some((char) => {
    const code = char.charCodeAt(0);
    return code <= 0x20 || code === 0x7f || FORBIDDEN_ASCII.has(char);
  });

/**
 * Parse a `resolve-addresses` name as `url::Host::parse` with the bare IPv6 fallback upstream
 * adds: an IP literal is answered directly; a domain is IDNA-processed to ASCII.
 */
export function parseHostName(
  name: string,
): { readonly ip: IpAddress } | { readonly domain: string } | undefined {
  if (name.startsWith("[")) {
    if (!name.endsWith("]")) return undefined;
    const ip = parseIpv6(name.slice(1, -1));
    return ip && { ip: ipAddress("ipv6", ip) };
  }
  if (name.includes(":")) {
    const ip = parseIpv6(name);
    return ip && { ip: ipAddress("ipv6", ip) };
  }
  if (!name || forbidden(name)) return undefined;
  let host: string;
  try {
    // The forbidden check above leaves nothing the URL parser could read as another component.
    host = new URL(`http://${name}/`).hostname;
  } catch {
    return undefined;
  }
  if (!host || forbidden(host)) return undefined;
  const ipv4 = parseIpv4(host);
  return ipv4 ? { ip: ipAddress("ipv4", ipv4) } : { domain: host };
}
