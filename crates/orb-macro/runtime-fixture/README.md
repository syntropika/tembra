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
monotonic clock, through the asynchronous bindings of the `wit-bindgen` the facade brings. Every
installation of the fixture grants the `random` and `clocks` system capabilities; the kernel
refuses one that does not.

Its `counting` service, in the `keyed` module, imports `example:counters/counter@1.0.0` by key,
from the package in `consumed/`: its asynchronous `count` acquires the provider's objects of two
keys through the view the macro derives, `counter_ref`, adds to them and reads them back, and its
`read` returns an object's tally, the provider's record, which the view's module names. Every
installation of the fixture binds the view to a provider; the test's is native, its objects each
count on their own and it refuses the key `refused`.

Its `readings` service imports the same interface plainly, which reaches the provider's `main`
object, and passes the provider's `tally` record on as it is: `read` returns it in a record of the
module, `total` and `joined` take it back, alone and in a list, and the service's state keeps the
last one, which the kernel stores as the JSON `serde` writes for the provider's record. The state
also holds a field under `#[cfg(any())]`, which no build compiles, and the `Default` the macro
writes for the state leaves the field out with it. Its `library` service takes and returns the
`book` and `topic` of `example:shelves/shelf@1.0.0`, in `consumed/deps/`, an interface the
fixture's `shelf` service implements: the types of the shelf's export. Every installation binds the
plain import to the provider too.

The fixture also imports `example:ledger/book@1.0.0`, from `consumed/deps/`, and uses none of it,
so the component does not import it; the contract `cargo tembra build` writes from the macro's WIT
imports it as written, an interface with a resource, so every installation binds it to a keeper of
the ledger, a native one in the test, which answers no method. The ledger's `tally` holds
resource handles, which `serde` cannot write. The bindings leave a type out of a state's `serde`
derives by its bare name, so they leave the provider's `tally` out too, and the macro gives it its
derives back by its full name for the state of `readings`.

Its `speller` service, in the `spelling` module, imports `example:texts/texts@1.0.0` plainly, from
`consumed/deps/`, an interface with a resource: `spell-job` takes a record of the module holding
the caller's builder, the imported interface's handle, which the runtime moves into the call as it
moves the handle alone, appends the job's word through the import and consumes the builder; `spell`
starts a builder of its own through the import, so the Orb calls every function of the interface.
Every installation binds the import to a provider; the test's is native and issues its builders as
capabilities.

Its `lending` service imports that same shelf interface by key, the interface the fixture
implements: a proxy reaching the shelves of another keeper, through the view `shelf_ref`. The view
`use`s the shelf's `book`, so the elaborated world imports the shelf it exports, for the types
alone; the contract tells that from a hook, an interface the world imports itself, and the
installation binds the view alone, to a native keeper in the test. A derived signature names the
shelf's types through the implemented interface's module, `shelf::Book`, the copy of the export,
while the view's methods take the copy of the import, `shelf_ref::Book`, into which `lending`
converts each book it lends.

Its `documents` service takes and returns free JSON, `serde_json::Value`, which crosses as a WIT
`string` holding its text: alone, in a list, in an option and in the `note` record, which the macro
keeps the author's, converted to and from the bindings' record of the text. Both builds read and
write the text with `serde_json`, the `no-std` one through its `alloc` feature.

Its `units` service takes and returns newtypes, `Unit(String)` and `Count(u32)`, which cross as
their fields, a WIT `string` and `u32`: alone, in a list, in a tuple and in the `measure` record,
which the macro keeps the author's. Each comes back through the newtype's `From`. The
`native-only-newtype` feature adds the module `refusing`, whose newtype only a refusing `TryFrom`
makes: a component build of the crate then fails, with an error that names the newtype, which the
test checks.

A component carries the WIT of one Orb module, so the crate's other Orb modules build components of
their own: each feature below, without the default features, which add `guest`, selects one. The
test builds each std and with `no-std`, into the fixtures' target directory, and calls it. It then
runs the module's native tests, in the module's file: the crate built for the host with `native`,
the facade's `host`, or `native-sdk` for `renamed`, whose tests install the module's `orb()` in a
native host:

- `keywords` selects the module `list`, whose names are keywords of WIT, which the WIT writes with
  its `%` escape: the world `%list`, whose contract is `contract/%list.wit`, the method `%record`
  and the flag `%type` of the field `r#type`;
- `configured` selects the module `notebook`, whose items under `#[cfg(feature = "archive")]` the
  macro expands once per value of the feature: without `archive`, `Notes` clears its notes; with
  it, `Notes` archives them onto the shelf of `Shelf`, whose struct and block the feature adds,
  and runs the hook that the sub-module `archiving` declares. The component, its WIT and its
  contract are those of the build's features, and so is `orb()`, which refuses the hook natively;
- `renamed` selects the module `tally`, whose crate names the facade `sdk`, with `crate = "sdk"`,
  and has no `tembra`. Its method under `#[cfg(feature = "archive")]` makes the macro expand the
  module once per value of the feature, each copy through `::sdk::orb`, and the component reaches
  its state and the call's `context` through that name too, as `orb()` does natively;
- `hooks` selects the module `gatehouse`, which declares a series hook and a bail hook from their
  input, and the waterfall `guard` of the package in `admission/` by its identity. The test's native
  contributions answer them; natively, `orb()` refuses the declarations;
- `versioned` selects the module `counting`, whose state is at version 1; with `revised`, its next
  revision, whose state is at version 2. The test reloads one component into the other; natively,
  one build holds one revision, whose state keeps its version across a reload of the same code;
- `lamps` selects the module `lamps`, whose root resource, at version 1, notes when it opens and
  closes, in the host's log in a component and in a journal natively, where the `context` serves
  only a call's methods; with `revised`, its next revision, whose resource is at version 2;
- `flagged` selects the module `holder`, which implements the interface of the package in
  `rights/`, whose `flags` the bindings generate without `wit-bindgen`'s `bitflags` feature. Its
  native build fails today, so the native fixture's `tests/flags.rs` runs its native test, ignored
  until then;
- `system` selects the module `system`, which imports WASI's system clock, the clocks' types, the
  CLI's environment and the insecure random source; natively, `orb()` refuses the imports;
- `io` selects the module `io`, which imports the filesystem's preopened directories, the sockets'
  types and the HTTP client: it reaches the filesystem's `types` by the bindings' path, since the
  sockets' `types` is the one it imports, and the test grants a filesystem, a network destination
  and an HTTP origin; natively, `orb()` refuses the imports;
- `steady` selects the module `steady`, with a state, a root resource and the `context`, which
  implements the interface of the state example's contract, whose `deps/` holds the kernel's
  package.

Run `just test-orb-macro` from the repository root to build both real components and execute
`executor-wasm/tests/orb_macro_runtime.rs`, one test on Cranelift and one on Pulley. Each test
reads both contracts from the WIT the macro embedded in the components, as `cargo tembra build`
does, since the world read from a component's types imports every interface a view `use`s, the
shelf included; compares their WIT and typed metadata, loads each guest once with the grants of
chance's WASI imports, then checks that the kernel refuses an installation without them, and runs
every service of both guests in one host: it greets, observes the walker's state, draws and waits
through chance, reaches the provider's objects by key, takes the tallies back, moves a builder into
`speller` inside a record, crosses the flags, passes the consumed records through `readings` and
`library`, lends books to the keeper's shelves by key, and verifies physical Store and kernel
cleanup. Explicit execution requires both fixture paths and fails if either is absent. The
macro-only facade dependency disables host features in both guest builds.

The target is `wasm32-unknown-unknown`. Linking `std` here demonstrates support for ordinary Rust
libraries; it does not provide operating-system I/O, threads or network capabilities.
