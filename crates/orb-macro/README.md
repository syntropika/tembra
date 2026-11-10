# tembra-orb-macro

`#[orb]` declares a compiled Orb from a Rust module. The module is the contract: the macro derives
the WIT from it and generates the bindings, so an author writes neither a `.wit` file nor a build
script. [`cargo tembra build`](../cargo-tembra/README.md) compiles the crate, extracts the same WIT
from the component and packages the artifact. The [Rust orb example](../../examples/rust-orb/README.md)
shows the whole path.

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

    /// The service: a unit struct with an impl block of public functions.
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
| a unit struct with an `impl` block of `pub fn`                        | an exported `interface`, in kebab-case                                       |
| each `pub fn` without `self`                                          | a `func`; name and parameters in kebab-case                                  |
| a struct with named fields                                            | a `record`                                                                   |
| an enum of empty variants                                             | an `enum`; with one payload per variant, a `variant`                         |
| `i32`, `u32`, `String`, `Vec<T>`, `Option<T>`, `Result<T, E>`, tuples | `s32`, `u32`, `string`, `list<T>`, `option<T>`, `result<T, E>`, `tuple<...>` |
| `///` comments                                                        | `///` comments in the WIT                                                    |

## What it generates

The macro rewrites the module with:

1. `pub const WIT: &str`, the derived WIT text;
2. the `no_std` prelude for the Wasm target, `#[global_allocator]` on `dlmalloc` and a
   `#[panic_handler]` that traps, under `#[cfg(target_arch = "wasm32")]`; on the host the crate
   builds with std, so `cargo check` and rust-analyzer work without a Wasm target;
3. `wit_bindgen::generate!({ inline: WIT, world })`, the official macro with the WIT as a string;
4. `pub use exports::...::{Stats}`: the author's record and enum definitions are replaced by the
   generated types with the same names and public fields, so the author's functions and the
   generated traits share one type;
5. the author's items, unchanged;
6. a private struct implementing the `Guest` trait of every interface by forwarding to the
   author's functions, and `export!`.

The crate must list `wit-bindgen` (features `macros` and `realloc`) and `dlmalloc` (feature
`global`) among its dependencies: the generated code names them.

## Limits

- Exports only. Imports of other Orbs, WASI interfaces and hook contributions are not expressed
  yet; they need the other party's WIT.
- Functions are associated functions without `self`. State lives in `static`s, as in the compiled
  fixtures of `executor-wasm`.
- Types: records, enums, variants and the scalar, string, list, option, result and tuple types.
  No `resource`, `flags`, `stream` or `future`.
- Replacing the author's types drops their `#[derive]`s.
- Keep no `wit/` directory in the crate: `wit-bindgen` loads it on its own and finds the package
  twice.
