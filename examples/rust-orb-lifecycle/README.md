# A Rust Orb on the `tembra:orb` lifecycle

The counter of `rust-orb-state` (draft PR #1), written by hand in Rust with no macro and no
`cargo tembra`, against `tembra:orb@0.1.0`: the private package this branch adds for
componentized JavaScript Orbs. It answers one question before the Rust-first work is rebased
here: can a Rust component use that package unchanged, instead of the PR's own `tembra:guest`?

On the native host it can, with no change to the kernel, the executor or the contracts crate.

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

The host packages it with `Contract::source_component_artifact`, the call that packages a
componentized JavaScript Orb. That call verifies the lifecycle export, the host imports and the
public world. The private package never enters `contract/`, so another Orb can consume the
contract as it is.

## The PR's kernel interfaces, here

| PR #1, `tembra:guest`         | here, `tembra:orb`                          |
| ----------------------------- | ------------------------------------------- |
| `state-init.declare`          | `host.state-declare`, called in `create`    |
| `state.read`, `state.update`  | `host.state-read`, `host.state-update`      |
| `state-init.validate`         | `lifecycle.validate`                        |
| `resources-init`, `resources` | `host.resource-declare`, `lifecycle.open`   |
| `context`                     | `host.instance-address`, `context-*`, `log` |
| `tembra:manifest` section     | `lifecycle.describe`                        |

The host side returns every refusal as a `result`, so the component decides what to do with it
instead of trapping.

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

## Not checked yet

The Pulley interpreter, the Node and mobile hosts, root resources (`open`/`close`), hooks and the
call context (`instance-address`, principal, cancellation).
