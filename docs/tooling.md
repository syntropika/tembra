# Build tools

Build tooling resolves JavaScript dependencies before the orb runs.
Import it from `@tembra/sdk/tooling` and run it with Bun.
The SDK and engines keep separate builds.

```ts
import { build, Platform } from "@tembra/sdk/tooling";

await build({
  builds: {
    web: { entry: "src/counter.ts", platform: Platform.Browser },
    server: { entry: "src/counter.ts", platform: Platform.Embedded },
  },
});
```

Each entry exports a default orb.
Each target produces one JavaScript file named with its content hash.
`build()` returns the path, hash, target and byte count.
Type checking and minification are enabled by default.

## Targets

| Target              | Available environment                                  |
| ------------------- | ------------------------------------------------------ |
| `Platform.Browser`  | Browser APIs and browser package exports.              |
| `Platform.Worker`   | Worker APIs; application code must avoid DOM access.   |
| `Platform.Node`     | Node APIs and explicitly listed `nativeExternals`.     |
| `Platform.Embedded` | V8 with Tembra's host operations; no Node or DOM APIs. |

Dependencies are bundled without shared chunks.
Node builtins and allowed native dependencies remain external for the Node target.
The build rejects unresolved runtime imports and output that requires extra assets.
An embedded bundle assigns its orb to `globalThis.__tembraOrb` for the V8 loader.
Other targets emit ESM.

Use services to expose environment capabilities, such as storage, to an embedded orb.
Building browser code does not make DOM APIs available inside a Worker.

For engine builds and tool versions, see [DEVELOP.md](../DEVELOP.md).
See [build implementation](../packages/ts/sdk/src/tooling/build.ts) and [target resolution](../packages/ts/sdk/src/tooling/resolver.ts).
