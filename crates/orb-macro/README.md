# tembra-orb-macro

`#[orb]` declares a compiled Orb from a Rust module. The module is the contract: the macro derives
the WIT from it and generates the bindings, so an author writes neither a `.wit` file nor a build
script. [`cargo tembra build`](../cargo-tembra/README.md) compiles the crate, reads the contract the
macro embedded in the component and packages the artifact. The
[Rust orb example](../../examples/rust-orb/README.md) shows the whole path; the
[hooks example](../../examples/rust-orb-hooks/README.md) adds a contribution to another Orb's hook;
the [state example](../../examples/rust-orb-state/README.md) gives a service kernel-managed state.

```rust
#![cfg_attr(target_arch = "wasm32", no_std)]
extern crate alloc;

#[tembra_orb_macro::orb(package = "example:greeter@1.0.0")]
mod greeter {
    use alloc::{format, string::String};

    /// Counts over a text.
    pub struct Stats {
        pub words: u32,
        pub chars: u32,
    }

    /// The service: a unit struct marked #[service], with an impl block of public functions.
    #[service]
    pub struct Greeting;

    impl Greeting {
        /// Greet by name.
        pub fn hello(name: String) -> String {
            format!("Hello, {name}!")
        }

        /// Words and characters of a text.
        pub fn measure(text: String) -> Stats {
            Stats { words: text.split_whitespace().count() as u32, chars: text.chars().count() as u32 }
        }
    }
}
```

## What the macro reads

| In the module                                                          | In the WIT                                                                    |
| ---------------------------------------------------------------------- | ----------------------------------------------------------------------------- |
| the module name                                                        | the `world`                                                                   |
| `package = "..."` on the attribute                                     | `package ns:name@version;`                                                    |
| `wit = "dir"` on the attribute                                         | the directory with the WIT packages the Orb consumes, relative to the crate   |
| a unit struct with `#[service]` and an `impl` block of `pub fn`        | an exported `interface`, in kebab-case                                        |
| each `pub fn` without `self`                                           | a `func`; name and parameters in kebab-case                                   |
| a struct with named fields, when a service method or a state uses it   | a `record`                                                                    |
| an enum, when a service method uses it                                 | an `enum` when every variant is empty, otherwise a `variant`                  |
| `i32`, `u32`, `String`, `Vec<T>`, `Option<T>`, `Result<T, E>`, tuples  | `s32`, `u32`, `string`, `list<T>`, `option<T>`, `result<T, E>`, `tuple<...>`  |
| a free `pub fn` with `#[hook("ns:name/hook@version", priority = n)]`   | `import` and `export` of that hook interface in the world                     |
| a struct with named fields and `#[state(service = Name, version = n)]` | `import tembra:guest/state` and `export tembra:guest/state-init` in the world |
| `///` comments                                                         | `///` comments in the WIT                                                     |

Nothing is exported by accident. A service is a unit struct marked `#[service]`; the `pub fn` of
its `impl` blocks are its methods, private functions stay private. A struct or enum enters the WIT
only when a service method or a state uses it, directly or through the fields of another exported
type; the other types of the module, and unit structs without the marker, remain ordinary Rust. A
method that names a type the module does not declare is a compile error, and so is a service with
the name of the module, because WIT refuses a world and an interface of one name.

## Contributing to a hook

A hook is declared by another Orb and described by a WIT interface with one callback taking one
input. To contribute, name the directory holding the provider's WIT package on the attribute and
mark a free `pub fn` with the hook's identity and, when it matters, a priority (default 0; lower
runs first):

```rust
#[tembra_orb_macro::orb(package = "example:filter@1.0.0", wit = "../wit")]
mod filter {
    /// Block negative values; let the others continue down the hook unchanged.
    #[hook("example:admission/guard@1.0.0", priority = -7)]
    pub fn block_negative(input: guard::Admission) -> guard::Admission {
        if input.value < 0 {
            return guard::Admission { value: input.value, blocked: true };
        }
        input
    }
    // ... at least one service, as above
}
```

The macro reads the callback's name from that WIT, imports and exports the interface in the world
as the portable hook contract requires, re-exports the generated module under the interface name so
the author writes `guard::Admission`, and forwards the callback to the function. The function's Rust
name is free; its single parameter and its result are the callback's. The priority travels in the
component, in a `tembra:hooks` custom section, and `cargo tembra build` packages it with the
artifact.

## Kernel-managed state

The kernel keeps one state per service object: a versioned value it retains across reloads and
whose changes it reports to whoever observes the object's reference. A struct with named fields
marked `#[state]` is that state for the objects of one service; it names the service when the
module has more than one, and a version (default 1) the kernel compares on reload:

```rust
#[tembra_orb_macro::orb(package = "example:counter@1.0.0")]
mod tally {
    /// The state of one counter object.
    #[state(service = Counter, version = 1)]
    pub struct Count {
        pub total: i32,
        pub steps: u32,
    }

    #[service]
    pub struct Counter;

    impl Counter {
        /// Add `by` to the count and return the new count.
        pub fn add(by: i32) -> Count {
            Count::update(|count| {
                count.total += by;
                count.steps += 1;
            })
        }
    }
}
```

The macro gives the state type two associated functions: `Count::read()` returns the state of the
object running the current call, `Count::update(|count| ...)` changes it and stores it, returning
the stored value. The initial value is `Default::default()`, which the macro implements field by
field. Behind them, the world imports the kernel's `tembra:guest/state` interface and exports
`tembra:guest/state-init`: when the kernel creates a service object it asks the component for the
declaration of that service's state, as JSON with its version, and binds it to the state it
retains for the object. The executor serves both interfaces itself; the installer requires and
grants nothing for them.

Two rules follow from the kernel, not from the macro. A service's state is reached only by the
methods of that service: a call runs on one object, and the kernel refuses an update from any
other context. An Orb's root is not an object, so a hook contribution, which runs in the root, has
no state to read or write; what a contribution counts stays in the component, in `static`s, as the
[hooks example](../../examples/rust-orb-hooks/README.md) shows. One state per service object:
group what an object owns in one struct, and let observers select the fields they care about.

## What it generates

The macro rewrites the module with:

1. `pub const WIT: &str`, the derived WIT text;
2. the same text as a `tembra:wit` custom section of the Wasm module, and the hook priorities as
   a `tembra:hooks` section. `cargo tembra build` reads them and packages the contract exactly as
   written here, with the `///` comments the binary type section loses and the hook imports the
   linker drops because nothing calls them;
3. the `no_std` prelude for the Wasm target, `#[global_allocator]` on `dlmalloc` and a
   `#[panic_handler]` that traps, under `#[cfg(target_arch = "wasm32")]`; on the host the crate
   builds with std, so `cargo check` and rust-analyzer work without a Wasm target;
4. `wit_bindgen::generate!({ inline: WIT, world, generate_all })`, the official macro with the WIT
   as a string, plus `path` when the Orb consumes other packages and, with a state, the kernel's
   `tembra:guest` package nested in the inline text and `serde` derives on the generated types;
5. `pub use exports::...::{Stats}`: the author's record and enum definitions are replaced by the
   generated types with the same names and public fields, so the author's functions and the
   generated traits share one type; and `pub use exports::...::guard;` for each contributed hook;
6. the author's items, unchanged;
7. a private struct implementing the `Guest` trait of every interface by forwarding to the
   author's functions, hook callbacks and the kernel's `state-init` callback included, and
   `export!`;
8. with a state, `Default` and the `read`/`update` functions on each state type, over a hidden
   module that maps the kernel's JSON to the type with `serde_json`.

The crate must list `wit-bindgen` (features `macros` and `realloc`) and `dlmalloc` (feature
`global`) among its dependencies, and `serde` (features `derive`, `alloc`) and `serde_json`
(feature `alloc`) when it has a state: the generated code names them.

## Limits

- Imports of other Orbs' services and of WASI interfaces are not expressed yet; a hook
  contribution is the only way to consume another Orb's WIT.
- Compiled Orbs cannot declare hooks: the kernel does not accept hook providers from components
  yet, so a provider stays native or TypeScript.
- Functions are associated functions without `self`. Besides a service's state, data lives in
  `static`s, as in the compiled fixtures of `executor-wasm`.
- A state is serialized by `serde`: the kernel sees the Rust field names and enum cases, not the
  camelCase spelling the executor gives WIT values on the portable boundary.
- Types: records, enums, variants and the scalar, string, list, option, result and tuple types.
  No 64-bit integers across the boundary, no `resource`, `flags`, `stream` or `future`.
- Replacing the author's types drops their `#[derive]`s.
- Keep no `wit/` directory in the crate: `wit-bindgen` loads it on its own and finds the package
  twice.
