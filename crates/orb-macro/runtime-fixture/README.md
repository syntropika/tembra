# Macro runtime fixture

This guest compiles the same WIT implementation in two modes through the public `tembra::orb`
macro. The default uses `std::collections::HashMap`, `String` and `format!` with the standard
library's allocator and panic handler; `no-std` selects `#![no_std]`, the macro's explicit
`no_std` option and the optional `dlmalloc` allocator. The interface and output remain identical.

Its `walker` service relies on the derives of the types that enter the WIT, which the generated
types replacing them take: a record, an enum and a variant derive `Default`, `PartialEq`, `Eq`,
`Hash` or `Ord`, and the kernel-managed state `Position` starts from its derived `Default`. The
default build counts distinct moves in a `HashSet`, the `no-std` one in a `BTreeSet`.

Its `chance` service imports `wasi:random/random` and `wasi:clocks/monotonic-clock` with the macro's
`import` option: it draws bytes from the host's secure random source and awaits a wait on its
monotonic clock, so both builds enable `wit-bindgen`'s `async` feature. Every installation of the
fixture grants the `random` and `clocks` system capabilities; the kernel refuses one that does not.

Its `counting` service, in the `keyed` module, imports `example:counters/counter@1.0.0` by key,
from the package in `consumed/`: its asynchronous `count` acquires the provider's objects of two
keys through the view the macro derives, `counter_ref`, adds to them and reads them back. Every
installation of the fixture binds the view to a provider; the test's is native, its objects each
count on their own and it refuses the key `refused`.

Run `just test-orb-macro` from the repository root to build both real components and execute
`executor-wasm/tests/orb_macro_runtime.rs` on Cranelift and Pulley. The test decodes both component
contracts, compares their WIT and typed metadata, calls each guest, observes the walker's state,
reaches the provider's objects by key, and verifies physical Store and kernel cleanup. It installs
each guest without the grants of chance's WASI imports, which the kernel refuses, then with them,
and draws and waits through them. Explicit execution requires both fixture paths and
fails if either is absent. The macro-only facade dependency disables host features in both guest
builds.

The target is `wasm32-unknown-unknown`. Linking `std` here demonstrates support for ordinary Rust
libraries; it does not provide operating-system I/O, threads or network capabilities.
