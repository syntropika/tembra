# Macro runtime fixture

This guest compiles the same WIT implementation in two modes through the public `tembra::orb`
macro. The default uses `std::collections::HashMap`, `String` and `format!` with the standard
library's allocator and panic handler; `no-std` selects `#![no_std]`, the macro's explicit
`no_std` option and the optional `dlmalloc` allocator. The interface and output remain identical.

Its `walker` service relies on the derives of the types that enter the WIT, which the generated
types replacing them take: a record, an enum and a variant derive `Default`, `PartialEq`, `Eq`,
`Hash` or `Ord`, and the kernel-managed state `Position` starts from its derived `Default`. The
default build counts distinct moves in a `HashSet`, the `no-std` one in a `BTreeSet`.

Run `just test-orb-macro` from the repository root to build both real components and execute
`executor-wasm/tests/orb_macro_runtime.rs` on Cranelift and Pulley. The test decodes both component
contracts, compares their WIT and typed metadata, calls each guest, observes the walker's state,
and verifies physical Store and kernel cleanup. Explicit execution requires both fixture paths and
fails if either is absent. The macro-only facade dependency disables host features in both guest
builds.

The target is `wasm32-unknown-unknown`. Linking `std` here demonstrates support for ordinary Rust
libraries; it does not provide operating-system I/O, threads or network capabilities.
