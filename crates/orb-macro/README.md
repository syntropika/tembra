# tembra-orb-macro

`#[orb]` declares a compiled Orb from a Rust module. The module is the contract: the macro derives
the WIT from it and generates the bindings, so an author writes neither a `.wit` file nor a build
script. [`cargo tembra build`](../cargo-tembra/README.md) compiles the crate, reads the contract the
macro embedded in the component and packages the artifact. The
[service example](../../examples/rust-orb-service/README.md) shows the whole path; the
[hook contribution example](../../examples/rust-orb-hook-contribution/README.md) adds a
contribution to another Orb's hook and keeps its counts in a resource of the Orb's root; the
[hook declaration example](../../examples/rust-orb-hook-declaration/README.md) declares a hook; the
[state example](../../examples/rust-orb-state/README.md) gives a service kernel-managed state.

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

| In the module                                                               | In the WIT                                                                                                                |
| --------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------- |
| the module name                                                             | the `world`                                                                                                               |
| `package = "..."` on the attribute                                          | `package ns:name@version;`                                                                                                |
| `wit = "dir"` on the attribute                                              | the directory with the WIT packages the Orb consumes, relative to the crate                                               |
| `context` on the attribute                                                  | `import tembra:guest/context` in the world: the call's principal and cancellation, the object's address, the log          |
| a unit struct with `#[service]` and an `impl` block of `pub fn`             | an exported `interface`, in kebab-case                                                                                    |
| `#[service("ns:name/interface@version")]` on a unit struct                  | `export` of that interface of a consumed package, implemented by the `impl` block                                         |
| `#[service(lifetime = host)]` or `#[service(linger_ms = n)]`                | nothing in the WIT: the ownership policy of the service's objects, in the manifest                                        |
| `import = "ns:name/interface@version"` on the attribute                     | `import` of that interface of a consumed package, bound to a provider at installation                                     |
| a struct with `#[wit_resource]` and an `impl` block of `pub fn`             | a `resource` of its service's interface: the `#[constructor]` function its constructor, the `&self` functions its methods |
| each `pub fn` without `self`                                                | a `func`; name and parameters in kebab-case                                                                               |
| a struct with named fields, when a service method or a state uses it        | a `record`                                                                                                                |
| an enum, when a service method uses it                                      | an `enum` when every variant is empty, otherwise a `variant`                                                              |
| `i32`, `u32`, `String`, `Vec<T>`, `Option<T>`, `Result<T, E>`, tuples       | `s32`, `u32`, `string`, `list<T>`, `option<T>`, `result<T, E>`, `tuple<...>`                                              |
| a free `pub fn` with `#[hook("ns:name/hook@version", priority = n)]`        | `import` and `export` of that hook interface in the world: a contribution                                                 |
| a unit struct with `#[hook("ns:name/hook@version", composition = c)]`       | `import` and `export` of that hook interface in the world: a declaration                                                  |
| a struct with named fields and `#[state(service = Name, version = n)]`      | `import tembra:guest/state` and `export tembra:guest/state-init` in the world, and a `record` when a method uses it       |
| a struct with `#[resource(version = n)]` and a function marked `#[open]`    | `import tembra:guest/resources` and `export tembra:guest/resources-init` in the world                                     |
| `#[validate]` on a `fn(&self) -> Result<(), String>` of a `#[state]` struct | nothing in the WIT: the state's rule, run on every update and on reload; `validated` in the declaration to the kernel     |
| `#[close]` on a `fn(self)` of a `#[resource]` struct                        | nothing in the WIT: run by the kernel at teardown                                                                         |
| `///` comments                                                              | `///` comments in the WIT                                                                                                 |

Nothing is exported by accident. A service is a unit struct marked `#[service]`; the `pub fn` of
its `impl` blocks are its methods, private functions stay private. A struct or enum enters the WIT
only when a service method or a state uses it, directly or through the fields of another exported
type; the other types of the module, and unit structs without the marker, remain ordinary Rust. A
method that names a type the module does not declare is a compile error, and so is a service with
the name of the module, because WIT refuses a world and an interface of one name.

Nothing is bound by name either. The functions the generated code calls by role, a state's rule,
a root resource's opening and closing and a WIT resource's constructor, carry a marker,
`#[validate]`, `#[open]`, `#[close]` or `#[constructor]`, whatever their names and visibility; a
function without a marker is ordinary Rust, and a marker on a function of the wrong shape, or of
a struct of another kind, is a compile error.

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
component, in a `tembra:manifest` custom section, and `cargo tembra build` packages it with the
artifact.

## Declaring a hook

An Orb that owns a hook marks a unit struct with the hook's identity and its composition, one of
`series`, `waterfall` and `bail`, and names the directory holding the hook's WIT package, as a
contributor does:

```rust
#[tembra_orb_macro::orb(package = "example:gate@1.0.0", wit = "../wit")]
mod gate {
    /// Each contributor may change the admission or block it, in plan order.
    #[hook("example:admission/guard@1.0.0", composition = waterfall)]
    pub struct Guard;

    pub struct Decision { pub value: i32, pub blocked: bool }

    #[service]
    pub struct Admissions;

    impl Admissions {
        /// Run the hook over a fresh admission and report what the contributors decided.
        pub fn admit(value: i32) -> Decision {
            let decided = Guard::run(guard::Admission { value, blocked: false });
            Decision { value: decided.value, blocked: decided.blocked }
        }
    }
}
```

The macro gives the struct `Guard::run(input)`, which hands the input to the kernel: the kernel
calls the contributions of other Orbs in plan order and returns the composed result, nothing for a
series hook, the input type for a waterfall, an option for a bail. The callback's shape must fit the
composition, and the macro checks it against the WIT. In the world the hook interface is imported
and exported, as for a contribution: the export is what the portable hook contract requires of a
provider, and the macro answers it with a callback that traps, because the contributions answer the
hook; the import is the typed entry to the plan. Which role an interface plays travels in the
`tembra:manifest` section next to the contribution priorities, and `cargo tembra build` packages
it; an Orb declares a hook or contributes to it, never both. Service signatures use the module's
own types, so a method that reports the hook's record converts it, as `admit` does.

## The lifetime of a service's objects

The kernel creates one object per key and principal that a reference names, and by default keeps
it while something references it. The attribute can choose otherwise, as a TypeScript service does
in its declaration:

```rust
/// One ledger, created when the host installs the Orb and kept until the host closes.
#[service(lifetime = host)]
pub struct Ledger;

/// A cache whose objects stay two and a half seconds after their last reference is released.
#[service(linger_ms = 2500)]
pub struct Cache;
```

`lifetime = host` pins the objects: the host creates the `main` object under its own principal
when it installs the Orb, and nothing releases it before the host closes; such an object is
acquired only as `main` by the principal `host`. `linger_ms = n` keeps an unreferenced object, and
the kernel state it owns, for `n` milliseconds before the kernel releases it, so a reference taken
again soon finds the same object. A host-lifetime object cannot linger. The policy is not WIT: it
travels in the component's `tembra:manifest` section, keyed by the interface identity, and
`cargo tembra build` packages it in the artifact, where the executor reads it as a source
bundle's own declaration.

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
        pub total: i64,
        pub steps: u32,
    }

    #[service]
    pub struct Counter;

    impl Counter {
        /// Add `by` to the count and return the new count.
        pub fn add(by: i64) -> Count {
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

A state may carry its own rule: a function of its `impl` block marked `#[validate]`, of the shape
`fn(&self) -> Result<(), String>`, under any name. Then `Count::update` returns
`Result<Count, String>`: the closure's result is validated before the kernel stores it, and a
refusal leaves the previous value in place. The same function answers the kernel on a reload, when
the new instance is asked whether it accepts the value it inherits: a refusal fails the reload and
the running revision keeps serving, as the [state example](../../examples/rust-orb-state/README.md)
shows for an update. A function named `validate` without the marker is ordinary Rust.

Two rules follow from the kernel, not from the macro. A service's state is reached only by the
methods of that service: a call runs on one object, and the kernel refuses an update from any
other context. An Orb's root is not an object, so a hook contribution, which runs in the root, has
no state to read or write; what a contribution needs or counts is a resource of the root, below.
One state per service object: group what an object owns in one struct, and let observers select
the fields they care about.

## Resources of the Orb's root

A resource is a value the kernel opens when the Orb is activated and closes when it is torn down,
in the order its dependencies require, and that every method and hook callback of the Orb can
use. A struct marked `#[resource]`, with a function marked `#[open]`, `fn() -> Self`, and
optionally one marked `#[close]`, `fn(self)`, in its `impl` block, is a resource of the Orb's root:

```rust
/// What this contributor observed.
#[resource]
pub struct Counts { pub seen: u32, pub blocked: u32 }

impl Counts {
    #[open]
    pub fn open() -> Counts { Counts { seen: 0, blocked: 0 } }
}

#[hook("example:admission/guard@1.0.0", priority = -7)]
pub fn block_negative(input: guard::Admission) -> guard::Admission {
    Counts::with(|counts| counts.seen += 1);
    // ...
}
```

The macro gives the type `Counts::with(|counts| ...)`, which gives the closure the open value and
returns its result; before the resource is opened, or after it is closed, it traps. Behind it, the world imports
`tembra:guest/resources` and exports `tembra:guest/resources-init`: when the kernel creates the
root it asks the component for its resource declarations (name and version), runs the `#[open]`
function during activation and the `#[close]` one at teardown; `with` asks the kernel for the handle of the open resource
first. The value lives in the component, one per instance, as the TypeScript guest's root
resources do: a reload gives the Orb a fresh instance whose resources the kernel opens again. A
resource struct enters the WIT only when a service method uses it; a resource may hold types the
WIT has no spelling for, such as a client of some granted I/O.

## The call's context

A method runs inside a call the kernel admitted, on an object the kernel created for a key and a
principal. With `context` on the attribute the module imports the kernel's `tembra:guest/context`
interface, re-exported as `context`, and reads that view:

```rust
#[tembra_orb_macro::orb(package = "example:audit@1.0.0", context)]
mod audit {
    use alloc::{format, string::String};

    #[service]
    pub struct Auditor;

    impl Auditor {
        /// Who is calling, and which object answers.
        pub fn whoami() -> String {
            let object = context::object();
            context::log(context::Level::Info, "whoami was asked");
            format!("{} calls {:?} of {}", context::principal(), object.key, object.principal)
        }
    }
}
```

`context::principal()` is the principal of the current call and `context::object()` the address
of the object running it: the key its reference was acquired with and the principal that acquired
it, or an empty key and the principal `host` in a hook contribution, which runs in the Orb's root.
`context::cancelled()` tells whether the caller cancelled the call, `context::cancel()` cancels it,
and `context::log(level, message)` writes a record the host's log observers receive, attributed to
this Orb by the host. The executor serves the interface itself, as it does the state: the installer
requires and grants nothing, and the world exports nothing for it. Outside a call, in a resource's
`open` for instance, the principal and the cancellation are not available.

## Implementing a shared interface

A plain `#[service]` derives a new interface from the `impl` block, in the Orb's own package.
`#[service("ns:name/interface@version")]` names an interface that exists already, in a package of
the directory `wit = "dir"` points to, and the `impl` block implements it:

```rust
#[tembra_orb_macro::orb(package = "example:keeper@1.0.0", wit = "../wit")]
mod keeper {
    use alloc::vec::Vec;

    /// Implements `book` of the shared package: every function of the interface.
    #[service("example:ledger/book@1.0.0")]
    pub struct Keeper;

    impl Keeper {
        pub fn add(entry: book::Entry) -> u32 { /* ... */ }
        pub fn total() -> i64 { /* ... */ }
        pub fn entries() -> Vec<book::Entry> { /* ... */ }
    }
}
```

The world exports that interface instead of a derived one, its generated module is re-exported
under the interface's name, so the author writes `book::Entry`, and the macro generates the bridge
from each WIT function to the `pub fn` of the same name. The interface is fixed by its WIT: the
`impl` provides every function, with its parameter and result types, and nothing else; a missing or
extra `pub fn` is a compile error and a wrong type fails to compile at the bridge. A resource of the
interface is implemented by a `#[wit_resource(service = Keeper)]` struct of the resource's name,
with its constructor and methods. The derived contract has no interface of its own, only the world,
and `cargo tembra build` nests the shared package whole after it: the kernel holds every keeper of
`example:ledger/book@1.0.0`, native or compiled, to one contract, and an installer binds a consumer
of the interface to either. See
[rust-orb-shared-interface](../../examples/rust-orb-shared-interface/README.md).

## Importing another Orb's interface

`import = "ns:name/interface@version"` on the attribute, repeatable, adds `import` of that
interface to the world and re-exports its generated module under the interface's name:

```rust
#[tembra_orb_macro::orb(
    package = "example:speller@1.0.0",
    wit = "../text/contract",
    import = "example:text/texts@1.0.0"
)]
mod speller {
    #[service]
    pub struct Spelling;

    impl Spelling {
        pub fn spell(word: String) -> String {
            let builder = texts::start();
            for letter in word.chars() {
                builder.push(letter);
            }
            builder.text()
        }
    }
}
```

The package comes from the directory `wit` names, here the contract the provider's own build wrote.
`texts::start` is the imported function; a resource it returns is an owned handle with the
resource's methods, destroyed in the provider when the handle drops. At installation the host binds
the import to a provider Orb with `Install::requires`, as for any dependency. The macro records the
directory in the manifest and `cargo tembra build` nests the imported package whole in the
contract, so the kernel checks the provider against the same signature. See
[rust-orb-wit-resource](../../examples/rust-orb-wit-resource/README.md).

## Exporting a WIT resource

A struct marked `#[wit_resource]` becomes a `resource` of the interface of the service whose
methods hand it out: the function marked `#[constructor]`, `fn(..) -> Self`, is its constructor,
the `pub fn` taking `&self` are its methods, the other `pub fn` its static functions; a `pub fn new`
without the marker is one of those.

```rust
/// A text under construction: the caller owns it and pushes one character per call.
#[wit_resource]
pub struct Builder { text: RefCell<String> }

impl Builder {
    #[constructor]
    pub fn new() -> Builder { /* the constructor */ }
    pub fn push(&self, ch: char) -> u32 { /* a method */ }
    pub fn text(&self) -> String { /* a method */ }
}

#[service]
pub struct Texts;

impl Texts {
    /// Start an empty text; the caller owns the returned handle.
    pub fn start() -> Builder { Builder::new() }
}
```

```wit
interface texts {
  resource builder {
    constructor();
    push: func(ch: char) -> u32;
    text: func() -> string;
  }
  start: func() -> builder;
}
```

The struct keeps its fields and its `Drop`. The value lives in the component; a method returning
the struct returns an owned handle, which crosses to the caller, native or compiled, and every
method call travels back under the caller's own admission; when the last owner drops the handle,
the kernel destroys the value, which runs `Drop`. WIT methods borrow `self`, so a method changes a
value through `Cell` or `RefCell`; a `&mut self` method is a compile error, and so is a function
taking a resource as a parameter: a resource is handed out, never taken. The resource belongs to
one interface, that of `#[wit_resource(service = Name)]` or of the module's only service; another
service of the module may return it, and its interface `use`s the type. See
[rust-orb-wit-resource](../../examples/rust-orb-wit-resource/README.md).

## What it generates

The macro rewrites the module with:

1. `pub const WIT: &str`, the derived WIT text;
2. the same text as a `tembra:wit` custom section of the Wasm module, and the manifest, the
   declared hooks with their composition, the contributions with their priorities, the
   services that chose a lifetime or a linger and the `wit` directory, as a `tembra:manifest`
   section. `cargo tembra build` reads them and packages the contract exactly as written here,
   with the `///` comments the binary type section loses, the hook imports the linker drops
   because nothing calls them, and every consumed package nested whole, not the part the
   component happens to call;
3. the `no_std` prelude for the Wasm target, `#[global_allocator]` on `dlmalloc` and a
   `#[panic_handler]` that traps, under `#[cfg(target_arch = "wasm32")]`; on the host the crate
   builds with std, so `cargo check` and rust-analyzer work without a Wasm target;
4. `wit_bindgen::generate!({ inline: WIT, world, generate_all })`, the official macro with the WIT
   as a string, plus `path` when the Orb consumes other packages and, with a state, the kernel's
   `tembra:guest` package nested in the inline text and `serde` derives on the generated types;
5. `pub use exports::...::{Stats}`: the author's record and enum definitions are replaced by the
   generated types with the same names and public fields, so the author's functions and the
   generated traits share one type; `pub use exports::...::guard;` for each contributed hook and each
   implemented interface, `pub use ...::guard;`, the import module, for each declared hook and
   each imported interface; `pub use ...::context;` with the `context` option;
6. the author's items, unchanged;
7. a private struct implementing the `Guest` trait of every interface, derived or implemented,
   by forwarding to the author's functions, hook callbacks and the kernel's `state-init` and
   `resources-init` callbacks included, with a trapping callback for each declared hook, and
   `export!`; the resource trait of each `#[wit_resource]` struct on the struct itself, its
   constructor and methods forwarded and the handles wrapped at the boundary; with a declared
   hook, `run` on its struct, calling the hook's import;
8. with a state, `Default` and the `read`/`update` functions on each state type, over a hidden
   module that maps the kernel's JSON to the type with `serde_json`, and the kernel's `validate`
   callback, which runs the state's `#[validate]` function on the value a reload would adopt;
9. with a resource, one slot in the component's memory per resource, filled by the `#[open]`
   function and emptied by the `#[close]` one, and the `with` function on each resource type.

The crate must list `wit-bindgen` (features `macros` and `realloc`) and `dlmalloc` (feature
`global`) among its dependencies, and `serde` (features `derive`, `alloc`) and `serde_json`
(feature `alloc`) when it has a state: the generated code names them.

## Limits

- WASI interfaces are not expressed. An Orb consumes another Orb's WIT by importing an
  interface, implementing one or contributing to a hook; selecting another object by key, as a
  TypeScript Orb does with `context.get`, is not expressed.
- A declared hook's types enter no service signature: a method that reports the hook's record
  returns a record of the module, converted from it.
- A service's functions are associated functions without `self`; only the methods of a
  `#[wit_resource]` struct take `&self`, never `&mut self`. Besides a service's state, a
  resource's fields and the root's resources, data lives in `static`s, as in the compiled
  fixtures of `executor-wasm`.
- Root resources, `#[resource]`, belong to the root, declare no dependencies on one another, and
  are opened in the order the kernel chooses; the `#[open]` and `#[close]` functions take no
  arguments besides the value.
- A state is serialized by `serde`: the kernel sees the Rust field names and enum cases, not the
  camelCase spelling the executor gives WIT values on the portable boundary.
- Types: records, enums, variants and the scalar, string, list, option, result and tuple types;
  64-bit integers cross the boundary exactly. A `resource` is a `#[wit_resource]` struct or the
  handle of an imported interface, handed out as an owned handle and never taken as a
  parameter. No `flags`, `stream` or `future`.
- An implemented interface is fixed by its WIT: the `impl` adds no function to it, and a record
  of a consumed package is not a state; a state copies it into a record of the module.
- Replacing the author's types drops their `#[derive]`s.
- Keep no `wit/` directory in the crate: `wit-bindgen` loads it on its own and finds the package
  twice.
