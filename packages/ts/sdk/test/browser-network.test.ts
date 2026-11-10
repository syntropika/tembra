import { expect, test } from "bun:test";
import { BrowserNetwork } from "../src/interpreter/browser-network.ts";
import { fetchHttpProvider } from "../src/interpreter/fetch-http-provider.ts";
import { decodeAsyncHostRequest } from "../src/interpreter/host-protocol.ts";
import { BrowserCapabilities } from "../src/interpreter/io.ts";

const web = fetchHttpProvider(async () => new Response(null));

test("HTTP grants validate exact normalized origins using the browser URL API", async () => {
  for (const origin of [
    "https://*.example.test",
    "https://example.test/path",
    "https://example.test/?",
    "https://example.test/#",
    "file:///tmp",
    "https://user:password@example.test/",
  ])
    await expect(
      BrowserCapabilities.open({ http: [{ provider: "web", origins: [origin] }] }, {}),
    ).rejects.toThrow();
  const io = await BrowserCapabilities.open(
    { http: [{ provider: "web", origins: ["https://API.example.test:443"] }] },
    { http: { web } },
  );
  try {
    // Standard `wasi:http` sends select a provider only for the exact granted origin.
    expect(io.network.provider(new URL("https://api.example.test/events"))).toBe(web);
    for (const url of [
      "http://api.example.test/",
      "https://api.example.test:8443/",
      "https://other.example.test/",
      "https://user@api.example.test/",
    ])
      expect(io.network.provider(new URL(url))).toBeUndefined();
    // The legacy JSON HTTP operations no longer exist: a hostile packet is refused at decode.
    for (const operation of ["http.send", "http.read", "http.close"])
      expect(() =>
        decodeAsyncHostRequest(
          JSON.stringify({ operation, url: "https://api.example.test/", id: 1 }),
        ),
      ).toThrow(`Unsupported asynchronous host request operation ${operation}`);
  } finally {
    await io.dispose();
  }
});

test("socket authority is exactly the bound provider's grants, with native address rules", () => {
  const network = new BrowserNetwork(
    {
      network: [
        {
          provider: "types",
          connect: ["127.0.0.1:80", "[::1]:443"],
          listen: ["0.0.0.0:0", "[::1%3]:8080"],
          datagrams: ["10.0.0.2:53"],
        },
        { provider: "lookup", connect: ["127.0.0.1:81"], resolve: true },
      ],
      http: [{ provider: "web", origins: ["https://api.example.test"] }],
    },
    {},
  );
  const types = network.sockets("types")!;
  const lookup = network.sockets("lookup")!;
  const v4 = (ip: number[], port: number) => ({
    family: "ipv4" as const,
    ip,
    port,
    flowInfo: 0,
    scopeId: 0,
  });
  const v6 = (ip: number[], port: number, flowInfo = 0, scopeId = 0) => ({
    family: "ipv6" as const,
    ip,
    port,
    flowInfo,
    scopeId,
  });
  const loopback6 = [0, 0, 0, 0, 0, 0, 0, 1];
  // N1: no merging across the two bound providers.
  expect(types.grants.allows(v4([127, 0, 0, 1], 81), "tcp-connect")).toBe(false);
  expect(lookup.grants.allows(v4([127, 0, 0, 1], 80), "tcp-connect")).toBe(false);
  expect(types.grants.resolve).toBe(false);
  expect(lookup.grants.resolve).toBe(true);
  expect(lookup.grants.tcp).toBe(true);
  // Exact SocketAddr identity: IPv6 spellings compare numerically; flow info and scope count.
  expect(types.grants.allows(v6(loopback6, 443), "tcp-connect")).toBe(true);
  expect(types.grants.allows(v6(loopback6, 443, 1), "tcp-connect")).toBe(false);
  expect(types.grants.allows(v6(loopback6, 443, 0, 1), "tcp-connect")).toBe(false);
  // Listen compares the IP only, with port 0 meaning any port; bind accepts listen addresses.
  expect(types.grants.allows(v4([0, 0, 0, 0], 9000), "tcp-listen")).toBe(true);
  expect(types.grants.allows(v6(loopback6, 8080, 7, 0), "tcp-listen")).toBe(true);
  expect(types.grants.allows(v6(loopback6, 8081), "tcp-listen")).toBe(false);
  expect(types.grants.allows(v6(loopback6, 8080, 0, 3), "tcp-bind")).toBe(true);
  expect(types.grants.allows(v4([0, 0, 0, 0], 0), "tcp-bind")).toBe(true);
  expect(types.grants.allows(v4([127, 0, 0, 1], 7), "tcp-bind")).toBe(false);
  expect(types.grants.allows(v4([1, 2, 3, 4], 5), "tcp-accept")).toBe(true);
  // UDP: implicit bind only with datagrams; peers are exact.
  expect(types.grants.allows(v4([0, 0, 0, 0], 0), "udp-bind")).toBe(true);
  expect(types.grants.allows(v4([10, 0, 0, 2], 53), "udp-send")).toBe(true);
  expect(types.grants.allows(v4([10, 0, 0, 2], 54), "udp-receive")).toBe(false);
  expect(lookup.grants.udp).toBe(false);
  // HTTP grants authorize no socket provider, and an unbound name has no access at all.
  expect(network.sockets("web")!.grants.tcp).toBe(false);
  expect(network.sockets(undefined)).toBeUndefined();
  for (const bad of ["127.0.0.1", "01.2.3.4:5", "[::1]", "[::1%x]:1", "1.2.3.4:65536"])
    expect(() => new BrowserNetwork({ network: [{ provider: "p", connect: [bad] }] }, {})).toThrow(
      "Invalid network grant address",
    );
});
