# Native WIT composition

Native Orbs import synchronous and asynchronous WIT functions and provider-owned resources through
the existing `Reference`, `Owned`, receiver and portable delivery paths. The host selects each
provider through the macro-generated `Providers` argument. WIT remains the contract of record.

## Calling conventions

A provider implements `func` with a synchronous Rust function and `async func` with an async Rust
function. Constructors remain synchronous providers. Imported Orb functions always return
futures in native and component builds, including constructors, static functions and methods of
resources. Their public Rust signatures take owned data parameters in both builds, including
strings, lists and the containers holding them. The component bindings stay private. There is no
blocking adapter, nested executor or polling loop.

For a synchronous `normalize: func(value: u64) -> u64`, shared consumer source uses:

```rust,ignore
pub async fn request(value: u64) -> u64 {
    codec::normalize(value).await
}
```

For a resource `decoder`, both builds use `codec::Decoder::new(seed).await`,
`codec::Decoder::zero().await` and `decoder.total().await`. An `async func` method such as
`next` is awaitable in both builds. Authors write `.await` on every imported Orb call. An importing
service method is authored `async fn`, so its derived WIT exports `async func` in both builds.
For fixed WIT, update that consumer export explicitly: the macro rejects an async implementation
of a synchronous export and never silently changes its contract. Rust rejects `.await` inside a
synchronous function, including calls reached through helpers.

Awaitability does not change the provider's calling convention. A component wrapper for a WIT
`func` calls the synchronous binding directly when its future is polled; it does not yield. A WIT
`async func` uses the actual asynchronous binding and can suspend, preserving cancellation and
receiver lifetime. With the pinned `wit-bindgen` version, both the consumer export and the import
it awaits must be `async func` for other component work to advance during that import. A native
reference can suspend admission even for a synchronous provider. The two executors therefore do
not promise identical interleaving just because the Rust syntax is the same. Keep pure providers
synchronous and declare operations that need suspension asynchronous.

## Ownership and delivery

The wrappers are not cloneable. Each returned handle is an existing `Owned` with its own kernel
claim and exact interface/version/resource identity. Resource methods borrow that owner as their
implicit receiver. Lists, options, tuples, results, records and variants retain resource identity;
resource-bearing records and variants use native copies in the imported interface's module.
Explicit owned arguments move the same owner, without duplicating a claim.

Dropping an imported wrapper during a callback defers its ownership release to that callback's
existing settlement checkpoint. An abandoned imported call is cancelled and joined there; root
cancellation can stop that wait while its physical work remains accounted by the runtime. Physical
receiver loans pin the provider value until that work ends. Cancellation uses the existing
cooperative `Reference` semantics. A provider may observe cancellation and finish its admitted
call successfully. Failed decoding, undelivered results and acquisition failures drop their owners.

Reload invalidates resources from the retired exporter revision. A subsequent method observes
`gone/resource`; released references and closed hosts retain their established kernel refusals.
Keyed imports remain service references, distinct from provider-owned WIT handles.

## TypeScript clients

`client({ wit, world, out })` generates awaitable typed clients for resource interfaces. For
`decoder`, constructors and statics live on the acquired client as `newDecoder` and `decoderZero`.
Methods live on the returned `Decoder`; `release()` explicitly closes its SDK ownership. A literal
`resourceType` identifies its nominal WIT type in TypeScript. Resource arguments consume that
ownership and invalidate stale aliases. Local and imported WIT aliases keep their original resource
identity and generate transparent TypeScript aliases. Resource release and service reference release
are separate.

The wrappers use the matched prepared delivery channel and existing SDK owners. They retain
`bigint`, camelCase record fields, `{ tag, val }` variants/results and `T | null` options, including
inside resource containers. Generation rejects method/name collisions and unsupported profiles.

Prepared browser components and the Rust RPC server provide this SDK capability channel. RPC
uses the same generated clients for native and compiled providers without a browser component;
see [WIT resources over RPC](rpc-resources.md). Data-only Node/mobile execution and ordinary JS
service handlers remain unsupported for resource calls. Data-only clients retain their transport.

## Limits

- Explicit `borrow<T>` parameters are excluded; implicit resource method receivers are supported.
- Native imports and generated clients do not add stream/future profiles, maps or fixed-length lists.
- Nested nullable options remain unsupported by the portable value representation.
- Resource results must belong to their defining interface. Foreign owned arguments obey the
  existing imported defining-interface rules.
- Native hook declarations, WASI imports and native exporter records/variants holding authored
  resource structs retain their previous restrictions.
- Imported functions and methods require an active importing callback. Root constructors and
  arbitrary application code use the existing explicit `Reference`/`Owned` APIs instead.

## Verification

`just test-native-fixture` executes macro-generated native consumers and the fixed-WIT decoder
provider, container delivery, physical cancellation/abandonment, failed acquisition and reload.
`just test-orb-macro` builds the same provider and consumer as components and runs all four
native/compiled combinations in `orb_macro_imports`. Acceptance builds these fixtures too.
`bun test packages/ts/sdk/test/client-tooling.test.ts` checks generated types and actual SDK owner
delivery, resource methods, moves and release. Run `just ci` for repository checks and
`just test-wasm-acceptance` separately for the complete artifact-dependent platform suite.

These commands describe executable coverage. A local run is not evidence of a CI run or of
platform qualification on a different host.
