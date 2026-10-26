# Code structure

For environment setup, tools and verification commands, see [DEVELOP.md](DEVELOP.md).

The core owns admission, versions, ownership and lifecycle order. Adapters execute callbacks and keep language values. Infrastructure owns transport and coordination. Build tooling runs outside the runtime.

## Modules

- Keep each module focused on one responsibility.
- Keep records and their index updates together.
- Use private modules before adding another crate or package.
- Keep public entrypoints stable. Internal code imports the owning module.
- Prefer files below 300 lines. Keep a cohesive dispatcher together when splitting would add noise.
- Keep one lifecycle engine in Rust. Do not copy its policy into JS.

## Layout

Use the project formatters. Leave one blank line between methods. Group related fields. Separate validation, mutation and publication when these steps have different guarantees.

Do not compress control flow to save lines. Avoid helpers that only rename one expression.

## Comments

Use simple technical English. Write short, active sentences. Put one idea in each sentence.

Give every production method a short doc sentence that states its purpose or guarantee. Use inline comments for constraints, reasons and lifetime rules. Do not restate the code. Keep long examples and design notes outside inline comments.

```rust
// Keep ownership until the callback settles.
```

## Verification

Run `just check` and browser acceptance after building WASM. Compare the same workloads when changing hot paths. Record the build profile, feature set and runtime. Keep performance reports outside the source tree.
