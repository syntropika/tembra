# A Rust Orb on the `tembra:orb` lifecycle, by hand

The counter of [rust-orb-state](../rust-orb-state/README.md), written by hand in Rust with no macro
and no `cargo tembra`, against `tembra:orb@0.1.0`: the kernel's private package, the one a
componentized JavaScript Orb is built against. It is the lifecycle
[`#[orb]`](../../crates/orb-macro/README.md) generates, spelled out: what the component exports and
imports, which world its bindings come from and how the host packages it. Read next to the state
example, it shows what the macro writes for a `#[state]` with a `#[validate]` rule.

## What the component does

`counter/src/lib.rs` exports two interfaces and imports one:

- `example:counter/counter` (export): the public service of `counter/contract/tally.wit`.
- `tembra:orb/lifecycle` (export): what the host calls around the service. `describe` says which
  services and methods the component implements, before anything else runs. `create` runs for
  the Orb root and for each counter object; the object declares its kernel state there.
  `validate` answers whether the state a reload retained is still acceptable. `release` forgets
  an object.
- `tembra:orb/host` (import): `state-declare`, `state-read`, `state-update`, `environment` (which
  object the current call serves) and `log`.

It is built against the public contract plus `tembra:orb/source`, the same world the TypeScript
build prepares with `Contract::source_world` before componentizing JavaScript:

```rust
wit_bindgen::generate!({
    inline: "
        package example:counter-build;

        world counter-orb {
            include example:counter/tally@1.0.0;
            include tembra:orb/source@0.1.0;
        }
    ",
    path: ["contract", "../../../crates/contracts/wit/tembra/orb.wit"],
    world: "example:counter-build/counter-orb",
    generate_all,
});
```

The macro forms the same build world, named `<world>-orb`, and adds an import of each hook the
module declares; it nests its own copy of the private package in the inline WIT instead of reading
the contracts crate. The host packages the component with `Contract::source_component_artifact`,
the call that packages a componentized JavaScript Orb and the one `cargo tembra build` makes. That
call verifies the lifecycle export, the host imports and the public world. The private package
never enters `contract/`, so another Orb can consume the contract as it is.

## Where the earlier `tembra:guest` interfaces went

Before it targeted `tembra:orb`, the Rust-first prototype had a kernel package of its own,
`tembra:guest@1.0.0`, and a `tembra:manifest` custom section. Each part has its place in the
lifecycle, here and in what the macro generates:

| earlier, `tembra:guest`       | now, `tembra:orb`                                                                  |
| ----------------------------- | ---------------------------------------------------------------------------------- |
| `state-init.declare`          | `host.state-declare`, called in `create`                                           |
| `state.read`, `state.update`  | `host.state-read`, `host.state-update`                                             |
| `state-init.validate`         | `lifecycle.validate`                                                               |
| `resources-init`, `resources` | `host.resource-declare`, `host.resource-read`, `lifecycle.open`, `lifecycle.close` |
| `context`                     | `host.instance-address`, `context-*`, `signal-cancelled`, `log`                    |
| `tembra:manifest` section     | `lifecycle.describe`                                                               |

Two parts had no counterpart there, because the executor routed them by itself: running a hook the
Orb declares, now `host.hook-declare` in the root's `create` and `host.select-hook` before each run,
and calling an imported interface, now `host.reference-declare` in the root's `create` and
`host.select` before each call. The host side returns every refusal as a `result`, so the
component decides what to do with it instead of trapping.

## Run it

```sh
./run.sh
```

```text
packaged            79900 bytes
observed state      {"steps":0,"total":0}
observed state      {"steps":1,"total":5000000000}
  log counter info: add 5000000000
add(5000000000)  -> ok {"steps":1,"total":5000000000}
observed state      {"steps":2,"total":4999999998}
  log counter info: add -2
add(-2)          -> ok {"steps":2,"total":4999999998}
add(-6000000000) -> refused: a total of -1000000002 is below zero
  log counter info: validate {"steps":2,"total":4999999998}
reloaded            component instances: 1
read()           -> {"steps":2,"total":4999999998}
observed state      {"steps":3,"total":4999999999}
  log counter info: add 1
add(1)           -> ok {"steps":3,"total":4999999999}
```

The reload creates a new component instance. The kernel keeps the object and its state, asks the
new instance's `validate` about it (the `validate` log line), and only then serves calls.

## What it leaves out

The counter has a state and nothing else: its root declares nothing, and `open` and `close` refuse
every callback. The macro's examples run the other parts of the lifecycle on the native host: root
resources in [rust-orb-hook-contribution](../rust-orb-hook-contribution/README.md), hooks in
[rust-orb-hook-declaration](../rust-orb-hook-declaration/README.md), references to an imported
interface in [rust-orb-import](../rust-orb-import/README.md) and the call context in
[rust-orb-context](../rust-orb-context/README.md). Not checked yet, here or there: the Pulley
interpreter and the Node and mobile hosts.
