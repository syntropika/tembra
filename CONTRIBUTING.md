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

## Invariants

A change keeps each invariant below. Each one names the tests that guard it, and the recipe in parentheses runs them. A change that adds a case to an invariant adds a test for it. A case that is known to break an invariant is named under it, with the pull request that fixes it, which removes the line.

- A build for wasm32 is a component, whatever the facade's features. A guest gets the facade's `host` feature when its `tembra` keeps the default features, or when another crate of the build enables it. Its component still answers each call as the guest built without `host` does.
  - `a_wasm32_guest_with_the_host_feature_answers_as_a_component` in `crates/executor-wasm/tests/orb_macro_runtime.rs`, which builds `crates/orb-macro/host-feature-fixture` both ways, std and `no_std` (`just test-orb-macro`).
  - `the_facade_checks_for_wasm32_with_macros` in `crates/orb-macro/tests/configurations.rs` (`just test-configurations`).
  - `a_freestanding_guest_builds_for_wasm32_whatever_links_std` in `crates/orb-macro/tests/configurations.rs`, which builds the `no_std` guest `crates/orb-macro/config-fixtures/freestanding` with no facade feature, `host` and `contracts` (`just test-configurations`).
- A part that the native side does not support never fails a build, whatever runtime features the build enables. The module compiles, and its `orb()` names the part. Only a host that installs the Orb fails.
  - `crates/orb-macro/native-fixture/tests/unsupported.rs` (`just test-native-fixture`).
  - `a_resource_with_a_constructor_beside_a_private_service_fails_the_installation` in `crates/orb-macro/native-fixture/tests/burrow.rs` (`just test-native-fixture`), and `a_resource_that_cannot_see_its_client_builds_as_a_component` in `crates/orb-macro/tests/configurations.rs`, on `crates/orb-macro/config-fixtures/hidden-client` (`just test-configurations`).
  - The native tests of the modules `gatehouse`, `system` and `io`, and of `notebook` with its `archive` feature, in `crates/orb-macro/runtime-fixture/src` (`just test-orb-macro`).
  - The native tests of the module `contributor` in `crates/orb-macro/runtime-fixture/src`, which `a_contribution_builds_with_or_without_the_runtimes_hooks` in `crates/executor-wasm/tests/orb_macro_runtime.rs` runs with the facade's `host` alone and with `hooks` (`just test-orb-macro`).
  - `a_workspace_whose_application_enables_host_checks_beside_a_guest` in `crates/orb-macro/tests/configurations.rs`, on the mixed workspace `crates/orb-macro/config-fixtures/workspace`, whose guests declare a hook and contribute to one beside an application that enables `host` without `hooks` (`just test-configurations`).
  - The native hosts in `crates/orb-macro/config-fixtures/native-hosts` (`just test-orb-both-ways`).
- Generated code reaches the facade only by the name that `crate = ".."` gives it, and reaches the module's items by paths that the module can see.
  - `examples/rust-orb-native/italian`, which builds as a component and natively (`just test-orb-examples`).
  - `crates/orb-macro/native-fixture/tests/trail.rs` (`just test-native-fixture`).
  - `a_renamed_facade_serves_the_copies_both_ways` in `crates/executor-wasm/tests/orb_macro_runtime.rs`, which builds the module `tally` of the runtime fixture both ways (`just test-orb-macro`).
- Only the library of the primary package writes `contract/`. The Orb modules of tests, benches, binaries and examples write nothing.
  - `the_other_targets_of_a_library_leave_its_contract_unchanged` in `crates/orb-macro/tests/configurations.rs`, on `crates/orb-macro/config-fixtures/library` (`just test-configurations`).
  - Known exception: an example built with `crate-type = ["lib"]` writes `contract/` too, since the macro does not tell it from the library. The macro's guide names it as a limit.
- Native and compiled Orbs answer the same codes and values for the same calls.
  - `the_native_build_and_the_component_answer_alike` in `crates/executor-wasm/tests/orb_macro_parity.rs` (`just test-orb-parity`). A new case is a scenario: a module or a function of `crates/orb-macro/parity-fixture`, and a row of its list.
  - `a_platform_port_answers_the_native_codes` in `crates/executor-wasm/tests/compiled_probe.rs`, for a compiled Orb served through a platform port (`cargo test --locked -p tembra-executor-wasm`), and `a_source_guest_that_rethrows_a_refusal_answers_its_code` in `crates/executor-wasm/tests/safety.rs`, for a source Orb that fails with a host's refusal (`just test-wasm-acceptance`, which builds the interpreter it needs).
- `Host` releases on close every callback it keeps, so that a callback holding the host does not keep it alive.
  - `closing_the_host_releases_its_observer` in `crates/tembra/tests/call_context.rs` (`cargo test --locked -p tembra`).
- Every option that the macro's guide, `crates/orb-macro/README.md`, documents has a test that builds it, both ways where the guide promises both.
  - The modules of `crates/orb-macro/runtime-fixture` (`just test-orb-macro`): the default one, `guest`, as a component only; each other one, under its feature, as a component, whose test calls it, and natively, where its native tests call it. The native test of `flagged` runs from `crates/orb-macro/native-fixture/tests/flags.rs` (`just test-native-fixture`). `just test-orb-both-ways` builds `guest` natively.
  - Each Orb crate of the examples and the fixtures, built the way the other recipes do not, and the native fixture, checked for wasm32 (`just test-orb-both-ways`).
  - The `cargo tembra` commands, run on the crates they write (`just test-cargo-tembra`).
- The full verification leaves the tree as it found it: no modified file, and no new file that `.gitignore` does not list.
  - `just check-clean-tree` ends each job of CI. `just ci` compares the tree after its parts with the tree before them.

## Verification

Run the checks of the area you change, and `just ci` for the full verification, as [DEVELOP.md](DEVELOP.md#checks) describes. `just ci` does not run `just test-wasm-acceptance`, the suite of the `acceptance` job of CI. Run it after building WASM, or run the browser tests alone with `bun run test:browser`. Compare the same workloads when changing hot paths. Record the build profile, feature set and runtime. Keep performance reports outside the source tree.
