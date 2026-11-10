# tembra-orb-macro

`#[orb]` declares a compiled Orb from a Rust module. The module is the contract: the macro derives
the WIT from it and generates the bindings, so an author writes neither a `.wit` file nor a build
script. [`cargo tembra build`](../cargo-tembra/README.md) compiles the crate, reads the contract the
macro embedded in the component and packages the artifact. The
[Rust orb example](../../examples/rust-orb/README.md) shows the whole path; the
[hooks example](../../examples/rust-orb-hooks/README.md) adds a contribution to another Orb's hook.

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

| In the module                                                         | In the WIT                                                                   |
| --------------------------------------------------------------------- | ---------------------------------------------------------------------------- |
| the module name                                                       | the `world`                                                                  |
| `package = "..."` on the attribute                                    | `package ns:name@version;`                                                   |
| `wit = "dir"` on the attribute                                        | the directory with the WIT packages the Orb consumes, relative to the crate  |
| a unit struct with `#[service]` and an `impl` block of `pub fn`       | an exported `interface`, in kebab-case                                       |
| each `pub fn` without `self`                                          | a `func`; name and parameters in kebab-case                                  |
| a struct with named fields, when a service method uses it             | a `record`                                                                   |
| an enum, when a service method uses it                                | an `enum` when every variant is empty, otherwise a `variant`                 |
| `i32`, `u32`, `String`, `Vec<T>`, `Option<T>`, `Result<T, E>`, tuples | `s32`, `u32`, `string`, `list<T>`, `option<T>`, `result<T, E>`, `tuple<...>` |
| a free `pub fn` with `#[contributes("ns:name/hook@version")]`         | `import` and `export` of that hook interface in the world                    |
| `///` comments                                                        | `///` comments in the WIT                                                    |

Nothing is exported by accident. A service is a unit struct marked `#[service]`; the `pub fn` of
its `impl` blocks are its methods, private functions stay private. A struct or enum enters the WIT
only when a service method uses it, directly or through the fields of another exported type; the
other types of the module, and unit structs without the marker, remain ordinary Rust. A method
that names a type the module does not declare is a compile error.

## Contributing to a hook

A hook is declared by another Orb and described by a WIT interface with one callback taking one
input. To contribute, name the directory holding the provider's WIT package on the attribute and
mark a free `pub fn` with the hook's identity:

```rust
#[tembra_orb_macro::orb(package = "example:filter@1.0.0", wit = "../wit")]
mod filter {
    /// Block negative values; let the others continue down the hook unchanged.
    #[contributes("example:admission/guard@1.0.0")]
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
name is free; its single parameter and its result are the callback's. Priorities, when they matter,
go in `Cargo.toml`: `[package.metadata.tembra] contributes = { "example:admission/guard@1.0.0" = -7 }`.

## What it generates

The macro rewrites the module with:

1. `pub const WIT: &str`, the derived WIT text;
2. the same text as a `tembra:wit` custom section of the Wasm module. `cargo tembra build` reads
   it and packages the contract exactly as written here, with the `///` comments the binary type
   section loses and the hook imports the linker drops because nothing calls them;
3. the `no_std` prelude for the Wasm target, `#[global_allocator]` on `dlmalloc` and a
   `#[panic_handler]` that traps, under `#[cfg(target_arch = "wasm32")]`; on the host the crate
   builds with std, so `cargo check` and rust-analyzer work without a Wasm target;
4. `wit_bindgen::generate!({ inline: WIT, world })`, the official macro with the WIT as a string,
   plus `path` and `generate_all` when the Orb consumes other packages;
5. `pub use exports::...::{Stats}`: the author's record and enum definitions are replaced by the
   generated types with the same names and public fields, so the author's functions and the
   generated traits share one type; and `pub use exports::...::guard;` for each contributed hook;
6. the author's items, unchanged;
7. a private struct implementing the `Guest` trait of every interface by forwarding to the
   author's functions, hook callbacks included, and `export!`.

The crate must list `wit-bindgen` (features `macros` and `realloc`) and `dlmalloc` (feature
`global`) among its dependencies: the generated code names them.

## Limits

- Imports of other Orbs' services and of WASI interfaces are not expressed yet; a hook
  contribution is the only way to consume another Orb's WIT.
- Compiled Orbs cannot declare hooks: the kernel does not accept hook providers from components
  yet, so a provider stays native or TypeScript.
- Functions are associated functions without `self`. State lives in `static`s, as in the compiled
  fixtures of `executor-wasm`.
- Types: records, enums, variants and the scalar, string, list, option, result and tuple types.
  No `resource`, `flags`, `stream` or `future`.
- Replacing the author's types drops their `#[derive]`s.
- `#[state]` is reserved and rejected: kernel-managed state for compiled Orbs needs the
  executor to provide it first.
- Keep no `wit/` directory in the crate: `wit-bindgen` loads it on its own and finds the package
  twice.
