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
#[tembra::orb(package = "example:greeter@1.0.0")]
mod greeter {
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

The public Rust facade re-exports this macro. A guest enables only the authoring entry point,
without linking the application's host runtime:

```toml
[dependencies]
tembra = { version = "0.2.2", default-features = false, features = ["macros"] }
```

The feature brings the `wit-bindgen` the generated bindings use, pinned to the version whose
rules the macro follows, so a guest does not name it; its `async` feature serves the bindings of
a `pub async fn`, of an import by key, whose `get` is asynchronous, and of a WASI interface that
waits.

A guest that depends on the facade under another name gives that name with `crate` on the
attribute, as serde's and tokio's attributes do; without it, the generated code reaches the facade
as `::tembra`. With

```toml
[dependencies]
sdk = { package = "tembra", version = "0.2.2", default-features = false, features = ["macros"] }
```

the module is marked `#[sdk::orb(package = "example:greeter@1.0.0", crate = "sdk")]`. The value is
the name as the guest's code writes it, a Rust identifier: `my_sdk` for a dependency named
`my-sdk`. The macro refuses another value at the literal.

The crate of a native-only module, see [using the Orb natively](#using-the-orb-natively), also
depends on `tembra` with `host` on native targets: its own code names the runtime.

## Standard library and freestanding guests

A guest uses Rust's standard library by default. The macro leaves its allocator and panic handler
to `std`, so the ordinary `String`, `Vec` and `format!` prelude works without `dlmalloc`. This also
allows dependencies that need `std`; a `wasm32-unknown-unknown` build still has that target's usual
limits on operating-system I/O, networking and threads. Linking `std` does not grant host access.

For a freestanding guest, opt in explicitly on both the crate and the Orb:

```rust
#![cfg_attr(target_arch = "wasm32", no_std)]
extern crate alloc;

#[tembra::orb(package = "example:greeter@1.0.0", no_std)]
mod greeter {
    use alloc::string::String;

    #[service]
    pub struct Greeting;

    impl Greeting {
        pub fn hello(name: String) -> String { name }
    }
}
```

That guest additionally lists `dlmalloc` with its `global` feature. The macro emits its allocator and trapping
panic handler only for the Wasm target; a host `cargo check` continues to use `std`. The `no_std`
option selects runtime ownership and does not change the WIT contract. A procedural attribute
cannot inspect the crate's `#![no_std]`, so the two declarations must agree. `cargo tembra new`
starts with this explicit freestanding mode.

## What the macro reads

| In the module                                                               | In the WIT                                                                                                                                                           |
| --------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| the module name                                                             | the `world`                                                                                                                                                          |
| `package = "..."` on the attribute                                          | `package ns:name@version;`                                                                                                                                           |
| `wit = "dir"` on the attribute                                              | the directory with the WIT packages the Orb consumes, relative to the crate                                                                                          |
| `context` on the attribute                                                  | `import tembra:guest/context` in the world: the call's principal and cancellation, the object's address, the log                                                     |
| a unit struct with `#[service]` and an `impl` block of `pub fn`             | an exported `interface`, in kebab-case                                                                                                                               |
| `#[service("ns:name/interface@version")]` on a unit struct                  | `export` of that interface of a consumed package, implemented by the `impl` block                                                                                    |
| `#[service(lifetime = host)]` or `#[service(linger_ms = n)]`                | nothing in the WIT: the ownership policy of the service's objects, in the manifest                                                                                   |
| `import = "ns:name/interface@version"` on the attribute                     | `import` of that interface of a consumed package, bound to a provider at installation                                                                                |
| `import = "wasi:name/interface@0.3.0"` on the attribute                     | `import` of that interface of a WASI package bundled with `tembra-contracts`, granted at installation                                                                |
| `import("ns:name/interface@version", keyed)` on the attribute               | `import` of a view of that interface, `interface-ref` in the package: a resource with the interface's functions as methods, and `get`, selecting an object by key    |
| a struct with `#[wit_resource]` and an `impl` block of `pub fn`             | a `resource` of its service's interface: the `#[constructor]` function its constructor, the `&self` functions its methods                                            |
| a `#[wit_resource]` struct or an imported `iface::Name` as a parameter      | an owned handle, `own<t>`, which the call takes from its caller; an imported interface's resource is `use`d from it                                                  |
| a consumed interface's type named through its module, `greeting::Entry`     | `use` of that type from its interface, in the derived interface naming it: the consumed package's type, which the bindings and the author's code share               |
| each `pub fn` without `self`                                                | a `func`; name and parameters in kebab-case                                                                                                                          |
| a struct with named fields, when a service method uses it                   | a `record`                                                                                                                                                           |
| an enum, when a service method uses it                                      | an `enum` when every variant is empty, otherwise a `variant`                                                                                                         |
| a case with named fields, `Refused { reason: String }`, of such an enum     | a `record` of its own, `<enum>-<case>`, the case's payload; the enum stays the author's, converted to and from the bindings' type, and so does every type holding it |
| a struct of `bool` fields with `#[flags]`, when a service method uses it    | a `flags`, a flag per field in kebab-case; the struct stays the author's, which the bindings take as it is                                                           |
| `i32`, `u32`, `String`, `Vec<T>`, `Option<T>`, `Result<T, E>`, tuples       | `s32`, `u32`, `string`, `list<T>`, `option<T>`, `result<T, E>`, `tuple<...>`                                                                                         |
| free JSON, `serde_json::Value`, by that path                                | a `string` that holds its JSON text, written and read at the boundary; a type holding it stays the author's, converted to and from the bindings' type                |
| a newtype, a tuple struct of one field, `struct Id(String)`                 | its field's type, `string`: the struct stays the author's, written as its field, read back through `From`, natively `TryFrom`; a type holding it stays the author's  |
| a free `pub fn` with `#[hook("ns:name/hook@version", priority = n)]`        | `import` and `export` of that hook interface in the world: a contribution                                                                                            |
| a unit struct with `#[hook("ns:name/hook@version", composition = c)]`       | `import` and `export` of that hook interface in the world: a declaration                                                                                             |
| a unit struct with `#[hook(composition = c, input = Type)]`                 | a hook `interface` of the package under the struct's name, its callback `apply` and the types the input reaches, imported and exported by the world: a declaration   |
| a struct with named fields and `#[state(service = Name, version = n)]`      | `import tembra:guest/state` and `export tembra:guest/state-init` in the world, and a `record` when a method uses it                                                  |
| a struct with `#[resource(version = n)]` and a function marked `#[open]`    | `import tembra:guest/resources` and `export tembra:guest/resources-init` in the world                                                                                |
| `#[validate]` on a `fn(&self) -> Result<(), String>` of a `#[state]` struct | nothing in the WIT: the state's rule, run on every update and on reload                                                                                              |
| `#[derive(..)]` on a struct or an enum that enters the WIT                  | nothing in the WIT: the generated type replacing it derives the same, besides what the bindings implement themselves                                                 |
| `#[close]` on a `fn(self)` of a `#[resource]` struct                        | nothing in the WIT: run by the kernel at teardown                                                                                                                    |
| an inline `mod`                                                             | nothing of its own: its items are read as the Orb's, grouped with what belongs to them                                                                               |
| `///` comments and `/** */` blocks                                          | `///` comments in the WIT, one per line                                                                                                                              |

Nothing is exported by accident. A service is a unit struct marked `#[service]`; the `pub fn` of its
`impl` blocks are its methods, private functions stay private. A struct or enum enters the WIT only
when a service method, a WIT resource's function or a hook's input uses it, directly or through the
fields of another type that enters; a state alone does not make a type enter. The other types of the
module, and unit structs without the marker, remain ordinary Rust, whatever they hold: only a type
that enters the WIT needs a WIT spelling for its fields and cases, so an array field or a case with
two unnamed payloads is a compile error there and nowhere else. A state that stays out of the WIT, and
every type of the module its fields reach that stays out too, get `serde`'s derives. A method that
names a type the module does not declare, other than a consumed interface's type named through
that interface's module, `greeting::Entry`, or free JSON, `serde_json::Value`, is a compile error, and so is a service with the name of
the module, because WIT refuses a world and an interface of one name. So is a method named `then`,
`release`, `state`, `ready`, `status` or `opaque`: every reference to a service object has those
members, and the kernel refuses such a method when it installs the Orb. The package and every
identity an attribute names are WIT identities, `ns:name@version` and `ns:name/interface@version`:
names of kebab-case words of lowercase letters and digits, each word starting with a letter, none
of them a WIT keyword, and a semantic version.

A name that enters the WIT is the kebab-case form of the Rust name, a raw identifier without its
`r#`: `HTTPRequest` is `http-request` and `r#type` is `type`. A WIT keyword, such as `list`,
`record` or `interface`, is written with WIT's `%` escape. The bindings name each item back from
its WIT name, `HttpRequest`, `get` for a function `GET`, `move_` for `r#move`, and the bridges speak
their names: a function or a resource keeps the author's name, and a record or an enum is
re-exported under it. A field or a case cannot be renamed so, because the generated type replaces
the author's: one the bindings would name otherwise, `userID` or a case `HTTP`, is a compile error
giving the name to use, `user_id` or `Http`. Inside an interface, types and functions share one
namespace in WIT, as do the methods and static functions of a resource: two items of one WIT name
there, a record `Stats` and a function `stats`, are a compile error naming both.

Nothing is bound by name either. The functions the generated code calls by role, a state's rule,
a root resource's opening and closing and a WIT resource's constructor, carry a marker,
`#[validate]`, `#[open]`, `#[close]` or `#[constructor]`, whatever their names and visibility; a
function without a marker is ordinary Rust, and a marker on a function of the wrong shape, or of
a struct of another kind, is a compile error.

A newtype, a tuple struct of one field without generic parameters, has no WIT type of its own:
wherever a type that enters the WIT names it, the WIT spells its field's type, so `struct
Code(String)` is a `string` and `struct Tagged(Entry)` is an `entry`. The struct stays the
author's, and so does a record or a variant holding one. A value crosses as its field, and comes
back through `From` of the field's type, which the author implements:

```rust
/// A unit of measure, by its symbol.
pub struct Unit(String);

impl From<String> for Unit {
    fn from(symbol: String) -> Self {
        Self(symbol)
    }
}
```

Natively, `TryFrom` of the field's type reads it instead, which a `From` implements too, so a
newtype may refuse a value there: a refused value fails that call alone, as its arguments, with
the error's message (see [Using the Orb natively](#using-the-orb-natively)). A component has no
such channel: it could refuse an argument before its method runs only by trapping, which would
retire its revision for every caller until a reload. So a component reads a newtype through
`From`, and a newtype that only a refusing `TryFrom` makes is native only: a component build of
the module fails, with an error that names the newtype and says why. A tuple struct of a name
another type of the Orb takes stays ordinary Rust, read by that name as before. A newtype holding a
resource, a newtype beside a resource in one parameter or result, and a newtype in a derived hook's
input are compile errors.

## Contributing to a hook

A hook is declared by another Orb and described by a WIT interface with one callback taking one
input. To contribute, name the directory holding the provider's WIT package on the attribute and
mark a free `pub fn` with the hook's identity and, when it matters, a priority (default 0; lower
runs first):

```rust
#[tembra::orb(package = "example:filter@1.0.0", wit = "../wit")]
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
artifact. A service's function names the hook's types the same way, `guard::Admission`, which its
interface `use`s from the hook's: see [the types of a consumed interface](#the-types-of-a-consumed-interface).

## Declaring a hook

An Orb that owns a hook marks a unit struct with the composition, one of `series`, `waterfall` and
`bail`, and the type of the callback's input; the macro derives the hook's interface from them,
under the struct's name in the Orb's package, as it derives a service's:

```rust
#[tembra::orb(package = "example:gate@1.0.0")]
mod gate {
    /// Each contributor may change the admission or block it, in plan order.
    #[hook(composition = waterfall, input = Admission)]
    pub struct Guard;

    pub struct Admission { pub value: i32, pub blocked: bool }

    #[service]
    pub struct Admissions;

    impl Admissions {
        /// Run the hook over a fresh admission and report what the contributors decided.
        pub fn admit(value: i32) -> Admission {
            Guard::run(Admission { value, blocked: false })
        }
    }
}
```

The interface `guard` holds the callback `apply`, whose result follows the composition, nothing
for a series hook, the input type for a waterfall, an option for a bail, and the types the input
reaches; the services borrow them with `use`, so `Admission` is one type for the whole module. The
macro gives the struct `Guard::run(input)`, which hands the input to the kernel: the kernel calls
the contributions of other Orbs in plan order and returns the composed result. In the world the
hook interface is imported and exported, as for a contribution: the export is what the portable
hook contract requires of a provider, and the macro answers it with a callback that traps, because
the contributions answer the hook; the import is the typed entry to the plan. The bindings spell the
hook's types once per side, so the macro generates the conversions `run` applies. Contributors read
the hook from the provider's contract, `wit = "../gate/contract"` and
`#[hook("example:gate/guard@1.0.0", priority = n)]`; see
[rust-orb-hook-declaration](../../examples/rust-orb-hook-declaration/README.md).

A hook that is a contract of its own, written by hand because no single Orb owns it or because its
provider is native, is declared by its identity instead, with the directory holding its package:
`#[hook("example:admission/guard@1.0.0", composition = waterfall)]` under `wit = "../wit"`. The
hook's types then belong to the consumed package, re-exported under the interface's name,
`guard::Admission`, and `run` takes and returns them; the macro checks that the callback's shape
fits the composition. Those are the types of the hook's import, which `run` calls; the world exports
the hook too, as the portable hook contract requires, and a derived interface takes the copy of the
types the export holds, another Rust type, so a service's function does not name
`guard::Admission`: it reports the hook's value in a type of the module, converted from it. A type
the hook `use`s from an interface the world only imports has no copy of its own: the import and the
export both name that interface's type, which a service's function names as the hook's module
does. A state holds the import's types as they are. Which role an interface plays travels in the
`tembra:manifest` section next to the contribution priorities, and `cargo tembra build` packages
it; an Orb declares a hook or contributes to it, never both. The world holds an interface in one
role: the macro refuses a hook's interface the module also implements or imports plainly, and an
interface imported or implemented twice, at the second import or item, naming both.

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
#[tembra::orb(package = "example:counter@1.0.0")]
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
the stored value. The initial value is the state's `Default`, which the macro implements field by
field unless the author provides it: an `impl Default for Count` anywhere in the Orb module, or a
derive of `Default` on the state, `#[derive(Default)]` or one implementing it under another name,
`SmartDefault`, which stays on the author's struct when the state stays out of the WIT and goes to
the generated type replacing it when the state enters the WIT, as every derive of a type that
enters it does (see [Limits](#limits)). That is how a state starts from a value of the author's
choosing:

```rust
impl Default for Count {
    fn default() -> Self {
        Count { total: 100, steps: 0 }
    }
}
```

The macro's `Default`, like a derived one, gives each field its type's `Default`, and follows a
field's `#[cfg(..)]` written plainly: one under `#[cfg_attr(..)]` is a compile error on the field,
since the macro writes the `Default` before the compiler evaluates the attribute, so derive or
implement `Default` for such a state. For the same reason, a derive of `Default` under
`#[cfg_attr(..)]` is a compile error on the attribute, since the macro's `Default` would be a
second one where it applies: derive `Default` without the attribute, or implement it. A field
whose type has none, an enum of the module for instance, is a compile error on that field:
implement `Default` for the state, as above, or give the field's type one, by hand or by a
derive, which for an enum marks the case it starts from:

```rust
#[derive(Default)]
pub enum Phase {
    #[default]
    Open,
    Closed,
}
```

Behind them, the world imports the kernel's `tembra:guest/state` interface and exports
`tembra:guest/state-init`: when the kernel creates a service object it asks the component for the
declaration of that service's state, as JSON with its version, and binds it to the state it
retains for the object. The executor serves both interfaces itself; the installer requires and
grants nothing for them. A native host keeps the same state, with the same JSON, see
[using the Orb natively](#using-the-orb-natively).

A state may carry its own rule: a function of its `impl` block marked `#[validate]`, of the shape
`fn(&self) -> Result<(), String>`, under any name. Then `Count::update` returns
`Result<Count, String>`: the closure's result is validated before the kernel stores it, and a
refusal leaves the previous value in place. The same function answers the kernel on a reload, when
the new instance is asked whether it accepts the value it inherits: a refusal fails the reload and
the running revision keeps serving, as the [state example](../../examples/rust-orb-state/README.md)
shows for an update. A function named `validate` without the marker is ordinary Rust. Every state
answers a reload, with a rule or without: the new instance first reads the value it inherits as its
own state type, so a value of another shape, a field added or renamed under the same `version`,
fails the reload instead of the first `read`.

The kernel keeps the state as JSON, which has no room for a NaN or an infinite float: `serde_json`
writes one as `null`, which reads back as no float, so a stored value holding one would fail every
later `read` and the reload. `update` checks that the JSON of the new value reads back as the
state's type before it sends the value to the kernel: a state with a rule returns the refusal from
`update`, as it returns the rule's, and any other state traps; the kernel keeps the previous value
either way. The initial value is checked the same way when the kernel creates the object, and one
that does not read back traps the creation. The JavaScript hosts, Node, mobile and the browser,
parse the state's JSON numbers as doubles, so an integer beyond ±2^53 loses precision there.

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
native host keeps the value across a reload instead, while the resource's name and version stay
the same, as it keeps the resources of every native Orb: nothing closes or opens, and the new
code's `with` gives the value the old code opened. The struct of another module is another Rust
type, even with the same fields, so a reload to another module whose resource keeps that name
needs a new version: otherwise each call of the new code answers `invalid/rust_type` before it
runs, as the runtime answers a hand-written Orb that reads the value as another type. At a new
version, `#[resource(version = 2)]`, the host opens the new code's resource with its `#[open]`
function, then closes the old value, once, through the old code's `#[close]` function. A resource
struct enters the WIT only when a service method uses it; a resource may hold types the WIT has no
spelling for, such as a client of some granted I/O. A native host opens the same resources, see
[using the Orb natively](#using-the-orb-natively).

## The call's context

A method runs inside a call the kernel admitted, on an object the kernel created for a key and a
principal. With `context` on the attribute the module imports the kernel's `tembra:guest/context`
interface, re-exported as `context`, and reads that view:

```rust
#[tembra::orb(package = "example:audit@1.0.0", context)]
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
#[tembra::orb(package = "example:keeper@1.0.0", wit = "../wit")]
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
from each WIT function to the `pub fn` of the same WIT name, `move_` or `r#move` for a function
`move`, named in the bridge as the bindings name it. The interface is fixed by its WIT: the
`impl` provides every function, with its parameter and result types, and nothing else; a missing or
extra `pub fn` is a compile error and a wrong type fails to compile at the bridge. A resource of the
interface is implemented by a `#[wit_resource(service = Keeper)]` struct of the resource's name,
with its constructor and methods; they too take and return what the WIT says, in the package's
types, `book::Entry` or an owned `book::Cursor` handle as a parameter, so no type of the module
enters the WIT through them. A function returning the resource, alone or in an `Option`, a
`Result`, a `Vec` or a tuple, returns the struct there, and the bridge wraps it into its handle. The
derived contract has no interface of its own, only the world, and `cargo tembra build` nests the
shared package whole after it: the kernel holds every keeper of `example:ledger/book@1.0.0`, native
or compiled, to one contract, and an installer binds a consumer of the interface to either. A
derived service of the same Orb takes and returns the interface's types as they are, `book::Entry`,
and a state holds them: see [the types of a consumed interface](#the-types-of-a-consumed-interface)
and [rust-orb-shared-interface](../../examples/rust-orb-shared-interface/README.md), whose state
keeps the entries.

## Importing another Orb's interface

`import = "ns:name/interface@version"` on the attribute, repeatable, adds `import` of that
interface to the world and re-exports its generated module under the interface's name:

```rust
#[tembra::orb(
    package = "example:welcomer@1.0.0",
    wit = "../greeter/contract",
    import = "example:greeter/greeting@1.0.0"
)]
mod welcomer {
    #[service]
    pub struct Welcoming;

    impl Welcoming {
        pub fn welcome(names: Vec<String>) -> Vec<String> {
            names.iter().map(|name| greeting::hello(name)).collect()
        }
    }
}
```

The package comes from the directory `wit` names, here the contract the provider's own build wrote.
`greeting::hello` is an ordinary function here and a call through the kernel at run time, under
the caller's principal; a resource an imported function returns is an owned handle with the
resource's methods, destroyed in the provider when the handle drops. Which Orb answers is the
installer's choice: it binds the import to a provider Orb with `Install::requires`, as for any
dependency, and the import reaches that provider's `main` object. The macro records the directory
in the manifest and `cargo tembra build` nests the imported package whole in the contract, so the
kernel checks the provider against the same signature. The interface's types are the module's to
pass on: a derived function takes or returns `greeting::Entry` as the imported functions do, see
[the types of a consumed interface](#the-types-of-a-consumed-interface). See
[rust-orb-import](../../examples/rust-orb-import/README.md) and, for a handed-out resource,
[rust-orb-wit-resource](../../examples/rust-orb-wit-resource/README.md). To reach the object of
another key, import the interface [by key](#importing-another-orbs-interface-by-key).

## Importing another Orb's interface by key

The kernel keeps one object per key and principal of a service. A plain import reaches the
provider's `main` object; `import("ns:name/interface@version", keyed)` on the attribute,
repeatable, reaches the object of a key the call chooses, as a TypeScript Orb does with
`context.get`:

```rust
#[tembra::orb(
    package = "example:router@1.0.0",
    wit = "../counters/contract",
    import("example:counters/counter@1.0.0", keyed)
)]
mod router {
    #[service]
    pub struct Routing;

    impl Routing {
        /// Add one to the count of the object of `key` and return its count.
        pub async fn bump(key: String) -> Result<u32, String> {
            let counter = counter_ref::get(vec![key]).await?;
            Ok(counter.add(1))
        }
    }
}
```

The macro reads the interface from the directory `wit` names, as for a plain import, and derives
a view of it, an interface of the Orb's package named after it followed by `-ref`, which the
world imports:

```wit
/// The objects of `example:counters/counter@1.0.0`, each selected by its key.
interface counter-ref {
  use example:counters/counter@1.0.0.{tally};
  /// An object of `example:counters/counter@1.0.0`: its methods are the interface's functions.
  resource counter {
    add: func(amount: u32) -> u32;
    read: async func() -> tally;
  }
  /// The object of `key` under the principal of the call, or why it was refused.
  get: async func(key: list<string>) -> result<counter, string>;
}

world router {
  import counter-ref;
  export routing;
}
```

The view's resource takes the interface's name, and its methods are the interface's functions,
with their parameters, results and documentation, `async` where a function is; the types they
name are `use`d from the interface, so they stay the provider's, and a derived function takes and
returns them through the view's module, `counter_ref::Tally`, as
[the types of a consumed interface](#the-types-of-a-consumed-interface) are. The view's generated
module is re-exported under the view's name: `counter_ref::get(key)` acquires the object of the key, the
`Vec<String>` of its parts, under the principal of the call, and returns a `counter_ref::Counter`
whose methods call that object. A key the kernel refuses, one the provider does not admit for
instance, is the `Err` of `get`, and the call goes on. One key reaches one object, through any
number of handles; another key or another principal reaches another object. `get` is
asynchronous: the function calling it is a `pub async fn`, `async` in the WIT as well, whose
bindings use the `async` feature of `wit-bindgen`, which the facade enables. A handle
belongs to the call's root: dropping it releases its claim when the call settles, and the root's
end expires every claim, a handle kept in a `static` included, so a later call cannot use it.

The manifest's `references` map the view to the interface it reaches, and `cargo tembra build`
packages that role in the artifact; the kernel checks the view's methods against the provider's
functions when the Orb is installed. The installer binds the view by its own identity, as it
binds a plain import:

```rust
let install = Install::default().requires("example:router/counter-ref@1.0.0", "counters", 1);
let router = engine.load(&artifact, install)?;
```

The objects of a view carry data: an interface with a resource, or a function taking or
returning a resource handle, a stream or a future, is not imported by key, nor one with a function
carrying data the runtime refuses in a signature, an option of an option, a map or a list of a
fixed length. Nor is an interface with a function `handle`, `take-handle` or `from-handle`, which
every object's bindings define for its handle, or with a parameter `self`, which names a method's
object. A plain import of the
same interface may stand next to the keyed one, re-exported under the interface's name. An
interface the module implements may be imported by key, and only so: a proxy serving the interface
reaches the objects of other providers through the view, and a plain import of it is an error on
the import, since a world that imports the interface itself and exports it is a hook, which
`#[hook(..)]` contributes to or declares. The view `use`s the interface's types,
so the elaborated world imports the interface it exports, for the types alone, which `cargo tembra
build` and the kernel tell from a hook, an interface the world imports itself. A derived signature
names such an interface's types through the implemented interface's module, `counter::Tally`, the
copy of the export the derived interface takes; the view's `counter_ref::Tally` is the import's,
which the view's methods take and return, and the macro refuses it in a derived signature. So does
a view of a hook the module contributes to or declares beside the keyed import, an interface the
world exports too: a derived signature names a contributed hook's types through the hook's module,
`guard::Admission`, and none of a declared hook's. A type such an interface `use`s from an
interface the world only imports has no copy of its own: the import, the export and the view all
name that interface's type, so a derived signature names it through the view and through the
interface's module alike, a declared hook's included. The
`counting` and `lending` services of the [runtime fixture](runtime-fixture/src/lib.rs) import by
key, `lending` the interface the fixture implements.

## Importing WASI interfaces

The host serves WASI 0.3 to a compiled Orb, under the grants of its installation. The same
`import` option imports one of its interfaces; the `wasi` namespace tells it from another Orb's:

```rust
#[tembra::orb(
    package = "example:dice@1.0.0",
    import = "wasi:random/random@0.3.0",
    import = "wasi:clocks/monotonic-clock@0.3.0"
)]
mod dice {
    #[service]
    pub struct Die;

    impl Die {
        /// A throw of the die, from the host's secure random source.
        pub fn throw() -> u8 {
            (random::get_random_u64() % 6) as u8 + 1
        }

        /// Wait on the host's monotonic clock, then throw.
        pub async fn throw_after(nanoseconds: u64) -> u8 {
            monotonic_clock::wait_for(nanoseconds).await;
            Die::throw()
        }
    }
}
```

The world imports each interface, `import wasi:random/random@0.3.0;`, and its generated module is
re-exported under the interface's name, `random` and `monotonic_clock` here, as for another Orb's.
The bindings' own path names it too, `wasi::random::random`, and every interface an import builds
on, such as `wasi::filesystem::types`, which `wasi:filesystem/preopens` brings: an Orb importing
`wasi:filesystem/preopens` and `wasi:sockets/types` reaches the filesystem's types by that path,
since two imports of one interface name, both `types`, would be re-exported under one name, which
the macro refuses, naming both, as any two modules it would re-export under one name.

The WASI packages are bundled with `tembra-contracts`, so a WASI import needs no `wit = "dir"`:
the macro reads the interface from its bundled package and nests that package, with the bundled
packages it builds on, in the WIT the bindings read. A consumed directory may hold some of them
already, as a contract `cargo tembra build` wrote holds those it depends on in its `deps/`: the
bindings read those from there. As the tools that load the directory require, every copy of a
bundled package the directory holds, whether the Orb uses it or not, must be a file of `deps/`
itself, either the bundled file line for line, comments included, whatever its line endings, or
an encoded WIT package that the tools merge with the bundled one: another copy there is an error
naming its file, and a copy in a directory of `deps/` or nested in the file of another package,
even line for line, is an error asking to remove it, and so is a copy of the kernel's package an
encoded package holds, in an Orb with a state, a resource or the context, for which the macro
brings that package itself. An encoded package holds only the parts of the bundled packages it
uses, and the bindings read those from it: an import that needs a part it lacks, the imported
interface whole or what a nested package uses, is an error on the import naming the copy, its
file and the part. The `wasi` namespace holds the bundled packages and nothing else: an identity
naming none of their interfaces, of another version or unstable like `wasi:clocks/timezone`, is a
compile error listing what there is.

WASI 0.3 is asynchronous where it waits: a clock's waits, files, sockets and HTTP are `async func`,
whose bindings are `async fn` a method awaits, as `throw_after` does, and standard input and output
are streams and futures. The bindings of such an interface use `wit-bindgen`'s `async` feature,
which the facade enables, even when its methods call only the interface's synchronous
functions, such as `monotonic_clock::now()`; `wasi:random` needs none.

Nothing binds a WASI import to another Orb: the installation grants it. `cargo tembra build`
writes the WASI packages the contract depends on into `contract/deps/`, as it writes the kernel's,
and its summary names the grant each import needs, here:

```text
WASI imports, grants the installer must give:
  system: clocks: wasi:clocks/monotonic-clock@0.3.0
  system: random: wasi:random/random@0.3.0
```

At installation the kernel binds each WASI import to the host's provider of that grant, as for
any compiled component, and refuses an installation that does not grant it: the installer of
`dice` sets `install.grants.system` to `SystemCapability::Random` and `SystemCapability::Clocks`,
and the methods then draw from the host's random source and wait on its monotonic clock. A
filesystem, socket or HTTP import needs the `FilesystemGrant`, `NetworkGrant` or `HttpGrant` of a
configured provider. An interface no host serves as an import, such as `wasi:cli/run` or
`wasi:http/handler`, which a component exports instead, is a compile error on the import naming
the interfaces of its package the hosts do serve; `tembra_contracts::host_serves` is the rule,
the one the installation's grant check applies. An interface of types only, `wasi:clocks/types`,
asks nothing of a host and is imported as it is.

## The types of a consumed interface

A derived signature names a type of a consumed interface through the module the Orb module
re-exports for that interface, as the author's code does: `greeting::Entry` for
`import = "example:greeter/greeting@1.0.0"`, `book::Entry` for an implemented
`#[service("example:ledger/book@1.0.0")]`, `guard::Admission` for a hook the module contributes
to, and `counter_ref::Tally` for a type the view of an interface imported by key uses, which is the
interface's own. A record, a variant, an enum, a flags or an alias of the interface is a parameter
or the result of a derived service's function or of a `#[wit_resource]` struct's constructor,
methods and static functions, alone or in an `Option`, a `Result`, a `Vec` or a tuple, and a field
or a case of a record or a variant of the module that enters the WIT:

```rust
#[tembra::orb(
    package = "example:archivist@1.0.0",
    wit = "../greeter/contract",
    import = "example:greeter/greeting@1.0.0"
)]
mod archivist {
    /// A greeter's entry, with when it was archived.
    pub struct Archived { pub entry: greeting::Entry, pub at: u64 }

    #[service]
    pub struct Archive;

    impl Archive {
        pub fn keep(entry: greeting::Entry, at: u64) -> Archived { Archived { entry, at } }
        pub fn latest(archived: Vec<Archived>) -> Option<greeting::Entry> { /* ... */ }
    }
}
```

```wit
interface archive {
  use example:greeter/greeting@1.0.0.{entry};
  /// A greeter's entry, with when it was archived.
  record archived {
    entry: entry,
    at: u64,
  }
  keep: func(entry: entry, at: u64) -> archived;
  latest: func(archived: list<archived>) -> option<entry>;
}
```

The derived interface naming the type, in its functions or in the types it defines, `use`s it from
the interface defining it, under the kebab-case form of its Rust name, renamed with `as` where its
WIT name differs, `a-b as ab` for `AB`. The bindings of the derived interface then name the type the
consumed interface's bindings generate, so the author's value crosses as it is, with no copy into a
type of the module and no conversion. Which type that is follows from the world: the types of an
interface the world only imports are in its import module, which the Orb module re-exports, and so
are those a view uses; an interface the world exports, an implemented one or a hook, has a copy of
its types in its export module, which a derived interface, exported too, takes, and which the Orb
module re-exports for an implemented interface and a contributed hook. The contract names the
consumed package, which `cargo tembra build` nests whole, as it does for the import or the export.

A consumed type implements what its bindings give it: a record or a variant `Clone`, `Copy` and
`Debug` as its contents allow, an enum the comparisons too and a flags `Hash` as well, none of them
`Default`. A derive of another trait on a type of the module holding one, `PartialEq` on
`Archived`, needs the author's `impl PartialEq for greeting::Entry`, written anywhere in the crate,
whose type the bindings' is; `Copy` on one holding a consumed type that holds a string or a list is
a compile error on the derive, naming the consumed type.

A state holds a consumed type too, in its fields or in the types of the module they reach. In an Orb
with a state the bindings give the consumed packages' records, variants and enums `serde`'s
derives, so the kernel keeps the value as the JSON `serde` writes for the bindings' type, with the
Rust names of its fields and cases. A state is a struct of the module: it holds a consumed type,
but is none.

What a derived interface or a state does not take from a consumed interface is a compile error on
the type that names it:

- a name the interface does not define, the error listing its types and resources, and through a
  view's module, `counter_ref::Count`, a type the view does not use, since it uses only the types
  the interface's functions name, the error listing those;
- a type that holds a resource handle, a stream or a future anywhere: a derived interface takes the
  consumed types that carry data only;
- a type that holds an option of an option, a map or a list of a fixed length anywhere, and an
  `Option` of a type that is an option, `Option<catalog::Opt>` for a `type opt = option<u32>`, all
  of which the runtime refuses in a signature;
- a resource of the interface, but an imported interface's handle taken as a parameter, alone or in
  an option, a result, a list or a tuple ([taking a resource](#taking-a-resource)), and an object
  of a view, `counter_ref::Counter`, which belongs to the call that acquired it;
- a type of a hook the module declares from a consumed package, whose `run` speaks the import's
  types where a derived interface takes the export's ([declaring a hook](#declaring-a-hook)), but
  one the hook `use`s from an interface the world only imports, which both name;
- a type of an implemented interface or of a hook the module contributes to named through the
  bindings' module of its import, `self::example::counters::counter::Tally`, or through a name a
  `use` binds to it, `counter::Tally` after `use super::example::counters::counter;`, the import's
  copy, where a derived interface takes the export's, which the interface's own module names,
  `counter::Tally`; a type the interface `use`s from an interface the world only imports has one
  copy, that interface's, which every module names;
- a type or a handle of a WASI interface, named through that interface's module or through a
  consumed interface that `use`s it from WASI, `files::Descriptor` for a
  `use wasi:filesystem/types@0.3.0.{descriptor};` in `files`;
- a type or a handle of an interface imported, plainly or by key, that `use`s types of an
  interface the world exports, one the module implements or a hook it contributes to or declares,
  directly or through other interfaces: the derived interface would `use` it from the import, and
  WIT refuses a world whose import depends on one of its exports;
- a consumed type in a derived hook's input, itself or in the types it reaches, which hold the
  module's own;
- in a state, a resource, a type holding a resource handle, and a consumed flags or a type holding
  one, which the bindings generate without `serde`'s derives;
- a type, a resource or a function of the module named like a consumed type in a derived interface
  that knows both, since an interface's types and functions share one namespace.

The runtime fixture's `readings` service takes and returns the record of an imported interface, and
its `library` service the records of an interface it implements; see
[runtime-fixture](runtime-fixture/README.md).

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
the struct, or an `Option`, a `Result`, a `Vec` or a tuple holding it, returns an owned handle in
its place, which crosses to the caller, native or compiled, and every method call travels back
under the caller's own admission; when the last owner drops the handle, the kernel destroys the
value, which runs `Drop`. WIT methods borrow `self`, so a method changes a value through `Cell` or
`RefCell`; a `&mut self` method is a compile error. A function of the interface takes the resource
back as a parameter, [below](#taking-a-resource). The resource belongs to one interface, that of
`#[wit_resource(service = Name)]` or of the module's only service, and only that interface returns
it: the runtime returns a resource only from the interface that defines it, so a function of
another service or of another service's resource returning it, alone, nested or in a record or a
variant, is a compile error on its result type. A record or an enum is defined in the interface
of the earliest service, in declaration order, that mentions it, in its functions or its
resources' functions, and the others `use` it from there. See
[rust-orb-wit-resource](../../examples/rust-orb-wit-resource/README.md).

## Taking a resource

A derived function takes a resource as a parameter, owned: a `#[wit_resource]` struct of its own
interface, `T`, which the WIT names `own<t>`, or the handle of an imported interface,
`texts::Builder`, named through the interface's module, which the derived interface `use`s from
the imported one. Either may sit in an `Option`, a `Result`, a `Vec`, a tuple, or a record or a
variant of the module, and a service's functions, a resource's constructor, its methods and its
static functions take them alike:

```rust
impl Builder {
    /// Append another builder's text to this one; the other builder is consumed.
    pub fn append(&self, other: Builder) -> u32 { /* ... */ }
}

impl Texts {
    /// Take a builder back and return its text.
    pub fn finish(builder: Builder) -> String { builder.text.into_inner() }
}
```

```wit
interface texts {
  resource builder {
    /* ... */
    append: func(other: builder) -> u32;
  }
  finish: func(builder: builder) -> string;
}
```

The caller's handle moves into the call, and the caller loses it: a native caller attaches its
`Owned` to the call's payload, a compiled one passes its handle. The bridge takes the handle back
into the author's value, which the function then owns and drops, running `Drop`, unless it keeps
it. An imported interface's handle crosses as the bindings' own type, the one the imported
functions return; the derived interface knows its resource by the kebab-case form of the handle's
Rust name, so a type or a resource of the module named like the handle is a compile error in an
interface that also defines or uses it. A record or a variant holding a resource is the bindings'
type, with the bindings' handle in the resource's place: a function taking one gets the handle,
whose `into_inner` gives the value.

The runtime moves an owned resource only into the interface that defines it, or into an Orb that
imports that interface, anywhere in an argument: a record or a variant of the module holds an
imported interface's handle where only parameters reach it. It refuses at installation today an Orb
whose function takes a borrowed resource, `borrow<t>` ("borrow requires a dedicated interpreter
value adapter"), so the macro derives none: `&T` is refused as every reference in a signature is. A
function taking a resource of another service's interface or of an implemented interface, itself or
in a record or a variant of the module, a WASI handle, also one an imported interface `use`s from
WASI, which the runtime moves between no Orbs, and a hook's input holding a resource are compile
errors too; so is a derived function returning an imported interface's handle, itself or in a
record or a variant of the module, or another service's resource, which the runtime returns only
from the interface that defines it.

## Flags

A struct of `bool` fields marked `#[flags]` is a WIT `flags`: each field is a flag, in declaration
order, named in kebab-case. Unlike a record, the struct is not replaced by the type the bindings
generate, a bitflags type with constants where the struct has fields: `wit-bindgen`'s `with` option
maps the WIT type onto the author's struct, so the bindings' functions, records, variants and
containers take and return the struct itself, with no conversion.

```rust
/// What a holder may do with a document.
#[flags]
#[derive(Default, PartialEq)]
pub struct Access { pub read: bool, pub write: bool, pub share: bool }

/// The access granted to someone.
pub struct Grant { pub who: String, pub access: Access }

impl Permits {
    /// The access with exactly the flags `access` lacks.
    pub fn invert(access: Access) -> Access { /* ... */ }
    /// The flags any of the grants sets.
    pub fn merged(grants: Vec<Grant>) -> Access {
        grants.iter().fold(Access::default(), |all, grant| all | grant.access)
    }
}
```

```wit
interface permits {
  /// What a holder may do with a document.
  flags access {
    read,
    write,
    share,
  }
  record grant {
    who: string,
    access: access,
  }
  invert: func(access: access) -> access;
  merged: func(grants: list<grant>) -> access;
}
```

The macro gives the struct what the bindings call on it, as `wit-bindgen` 0.63 lowers and lifts a
flags: the functions `empty()`, `bits()` and `from_bits_retain(bits)`, hidden from the
documentation, and `BitOr`, the union of two values, which the author's code may use too. These
names are the bindings', so a function of one of them in an `impl` block of the struct, or the
author's own `BitOr` of the struct with itself, `BitOr`, `BitOr<Self>` or `BitOr<T>` for a type
parameter `T`, is a compile error, in any module of the Orb, naming the struct by its path or by a
name a `use` brings in; a `BitOr` with another right-hand type, `BitOr<u32>`, does not overlap and
stays. The macro also derives `Clone`, `Copy` and `Debug`, which
a generated record or variant holding the struct needs of it, and, in an Orb with a state, serde's
`Serialize` and `Deserialize`, so a state can hold the struct, directly or in a record; each unless
the author derives or implements it. A state's JSON holds every field of the struct,
`{"read": true, "write": false, "share": false}`, where the portable boundary spells a flags value
as an object of its set flags in camelCase, `{"read": true}`, a flag left out being unset.

A flags holds one to 32 flags, as the component model rules, and its struct takes no generic
parameter; a flags struct of a sub-module is visible to the Orb module, as are the modules between
them, since the bindings name it from there through each, as the next section says. A derived
hook's input holds no flags: the bindings name a hook's types from its import module and from its
export module, at two depths, and the macro maps a flags onto the author's struct through one path.
The macro reads the struct before the compiler evaluates `#[cfg(..)]` and `#[cfg_attr(..)]`: the
struct or a field under `#[cfg(..)]` and a derive under `#[cfg_attr(..)]` are compile errors. A
struct marked `#[flags]` that no function uses stays ordinary Rust.

## Organizing the module in sub-modules

The Orb module may hold sub-modules, written inline; the macro reads them as part of the Orb and
leaves them in place, so a service can live with what belongs to it:

```rust
#[tembra::orb(package = "example:ledger@1.0.0")]
mod ledger {
    pub mod pinned {
        /// The balance of one pinned ledger object, kept by the kernel.
        #[state]
        pub struct Balance { pub total: i64 }

        #[service(lifetime = host)]
        pub struct Pinned;

        impl Pinned {
            pub fn add(amount: i64) -> i64 { Balance::update(|balance| balance.total += amount).total }
        }
    }

    pub mod passing { /* its own Balance, its own service */ }
}
```

A state or a WIT resource that names no service belongs to the one service of its sub-module, else
to the one service of the Orb. A service's methods are the `pub fn` of the `impl` blocks of its own
module, and a marked function goes in the module of its struct. The bridges the macro generates for
an item go in the item's module, so the author's types and functions resolve where they were
written: a WIT resource's bridge, for a derived or an implemented interface, sits next to its struct
and calls its `#[constructor]` whatever its visibility. What the Orb module itself reaches, a state
and its fields, a root resource, a WIT resource, a state's `#[validate]` function and a root
resource's `#[open]` and `#[close]` functions, is visible to the Orb module: `pub` or `pub(crate)`
at any depth, `pub(super)` one level down only. The macro names such an item through every module
of its path, from the Orb module, and a WIT resource from its service's module, so each of those
modules is visible there too: a WIT resource may sit below a private module that also holds its
service. A `pub use` re-exporting the item past a module hidden there does not help. A name that
enters the WIT is one across the modules, because the package has one namespace: the services, the
hooks, the resources and the types a method or a hook mentions; a state that stays out of the WIT
may repeat its name module by module, as `Balance` does, and a module's own name wins when a method
mentions it. The modules the macro re-exports in the Orb module, `context`, an imported interface, a
hook declared from a consumed package, are reached from a sub-module through `super::`. See
[rust-orb-service-lifetime](../../examples/rust-orb-service-lifetime/README.md).

## Using the Orb natively

The same module serves a native host: the macro also generates
`orb() -> tembra::Result<tembra::Orb>`, the Orb a host installs with `Host::install`, from the same
reading of the module as the component. Each `#[service]` is a `Service` named
`<orb>::<interface identity>`, with the signature of its interface, the lifetime and linger of its
attribute, and one method per `pub fn` under its WIT name. A local caller passes the arguments as a
tuple of the parameters in WIT order, `()` without parameters, and gets the Rust value back:
`Reference::call` checks the types only at run time, and another type fails the call with
`rust_type`. So for each service the macro also generates a typed client in the service's module,
as visible as its struct, `MathClient` for `Math`: `MathClient::new(reference)` wraps an
`Rc<Reference>` to one of the service's objects, and each function of the interface, and the
constructor and static functions of its WIT resources, as below, is an async method that takes the
call's `&Context` first and then the function's parameters, which the compiler checks. A function
named `new` is `new_` on the client, since `new` builds the client; its WIT name stays `new`. The
client of a service in a private module is as unreachable as the module: make the module `pub` for
a host to use it, or re-export the client where the runtime is linked, since a component build has
none. A portable caller's JSON array of arguments goes through a codec the macro writes from the
WIT, so the author's types need no serde. The Orb's name and API version
come from `[package.metadata.tembra]` of the crate manifest, with the defaults of
`cargo tembra build`: the crate name and 1. `orb_named(name, api_version, ..)` builds the same Orb
under a name and API version the host chooses at run time, so that a host can also install the
crate twice; `orb(..)` only calls it with the manifest's, passing its own arguments on. Both return
the runtime's `Orb`, an ordinary builder: before installing it, a host may append services written
by hand and the requirements they need.

```rust,ignore
#[tembra::orb(package = "example:calculator@1.0.0")]
pub mod calculator {
    #[service]
    pub struct Math;

    impl Math {
        pub fn add(a: u32, b: u32) -> u32 {
            a + b
        }
    }
}

// In a native host, under a root context.
host.install(vec![calculator::orb()?])?;
let math = host.get("calculator::example:calculator/math@1.0.0", vec!["main".into()], "host")?;
let math = Rc::new(math);
let sum: u32 = math.call(&context, "add", (1u32, 2u32)).await?;
let sum: Value = math.call_portable(&context, "add", json!([1, 2])).await?;
let sum: u32 = calculator::MathClient::new(math).add(&context, 1, 2).await?;
```

The native items sit inside `tembra::__host!`, a hidden macro of the facade that expands its input
only where the facade's `host` feature is on, and Cargo unifies features across a build: a native
host that depends on the Orb crate and on `tembra` with `host`, the default, gets `orb()`; a
component build enables only `macros`, gets none of it, and the component stays the same. The
generated code names the facade as `::tembra`, so an Orb crate depends on it under that name and
needs no dependency of its own for the runtime or its JSON; a host whose `tembra` lacks `host` finds
no `orb()`. The crate is rebuilt when its manifest changes, so `orb()` follows the table. See
[rust-orb-native](../../examples/rust-orb-native/README.md), where one host links two Orb crates
that export the same interface.

A native host can lend the Orb values a component cannot be given. The struct marked `#[inputs]`
is the argument of `orb(..)`, and of `orb_named(..)` after the name and API version, one value for
every service of the Orb, which the host builds: `notebook::orb(notebook::Inputs { .. })`. It stays the author's struct, out of the WIT, so its
fields may hold anything, such as `Rc` handles or closures. An Orb has at most one, visible to the
Orb module, without generic parameters.

A service reads them in its function marked `#[create]`,
`fn(&Inputs, &Declaration) -> tembra::Result<Self>`, which builds each object of the service, a
key and a principal, before its first call, and builds it again for the new code when the host
reloads the Orb with `Host::reload`. The `Declaration` gives the object's address and declares its
state, its resources and its tasks. What the create returns is the object's environment: the
service struct may then have fields, and its methods borrow them through `&self`, a receiver the
WIT leaves out. Without a `#[create]`, a service stays a unit struct whose methods take no `self`.
Only a native host builds such objects, so the component has no bridge for the service.

```rust,ignore
#[service]
pub struct Notes {
    title: String,
}

impl Notes {
    #[create]
    fn create(inputs: &Inputs, declaration: &Declaration) -> tembra::Result<Self> {
        let address = declaration.address()?;
        Ok(Self { title: (inputs.title)(&address) })
    }

    pub fn title(&self) -> String {
        self.title.clone()
    }
}
```

What every object of the Orb shares comes from the free function marked `#[root]`,
`fn(&Inputs, &Declaration) -> tembra::Result<T>`, the Orb's create: `orb(..)` hands it to
`Orb::create`, so the host runs it once when it installs the Orb, before any object is built. Its
`Declaration` declares what the Orb holds for its lifetime: its resources, with `resource_async`
for an asynchronous open that depends on other resources or an asynchronous close, its background
tasks, and the references it keeps. A `#[create]` reads the value as an `Rc<T>` with
`declaration.orb::<T>()`, and may keep it for its methods. An Orb has at most one, visible to the
Orb module.

```rust,ignore
pub struct Catalogue {
    books: Cell<u32>,
}

#[root]
fn catalogue(inputs: &Inputs, declaration: &Declaration) -> tembra::Result<Catalogue> {
    Ok(Catalogue { books: Cell::new(0) })
}

impl Shelf {
    #[create]
    fn create(inputs: &Inputs, declaration: &Declaration) -> tembra::Result<Self> {
        Ok(Self { catalogue: declaration.orb::<Catalogue>()? })
    }
}
```

`Host::install` commits the Orb before it runs the root, so a root that fails makes `install`
return its error and leaves the Orb installed: installing it again is refused with the code
`invalid` and the reason `orb_installed`. `Host::reload` runs the root and the creates again for
the new code, but refuses with the code `unsupported` and the reason `task_restart` while a task
that the root or an object declared is still running, such as the task of the native fixture's
`worker`, which runs until the host closes.

A native-only module, such as one with `#[inputs]`, may export nothing: an Orb whose only content is
its `#[root]`, or root resources, runs what they declare while the host keeps it installed, such as
a background task. Its WIT is then an empty world, and `orb(..)` registers no service.

Likewise, a service that its `#[create]` builds may have no method: its objects only run what the
create declares, such as tasks, while they are referenced. Its interface is empty in the WIT. A
contract plans no service for an empty interface, so `orb(..)` registers the service without a
signature, beside the services of its contract's plan.

Like any native-only module, a module with a `#[create]` or a `#[root]` serves only a native host.
They name `Declaration` and `tembra::Result`, which the facade has only with `host`, so the crate
depends on `tembra` with `host` on native targets and compiles the module only there, under
`#[cfg(not(target_arch = "wasm32"))]`, as the native fixture's manifest and its `notebook` do:

```toml
[dependencies]
tembra = { version = "0.2.2", default-features = false, features = ["macros"] }

[target.'cfg(not(target_arch = "wasm32"))'.dependencies]
tembra = { version = "0.2.2", default-features = false, features = ["macros", "host"] }
```

`#[service(key = main)]` and `#[service(principal = "name")]`, alone or together, guard the
address of the service's objects: the native factory refuses an object at another key than
`["main"]` with the code `forbidden` and the reason `key`, or of another principal with the reason
`principal`, before the create runs. A component could not check them, so they make the module
native only. A host-lifetime object belongs to the principal `host`, so `lifetime = host` takes no
other principal.

A method that returns `tembra::Result<T>` answers `T`, which is all its WIT result carries, or
fails the call with its error, for a local and a portable caller alike: `tembra::Result` is the
call's error channel, which a method that uses `?` on the runtime's results needs, such as the
value of a resource its environment holds. Any other `Result<T, E>` is a value, a WIT `result`.
The runtime's `Result` exists only where the facade links the runtime, so a module with such a
method is native only.

A method may also take the call's `&Context` among its parameters, for the caller's principal or
the call's cancellation. The WIT, the codec and the tuple of arguments leave it out, and the native
side passes the call's own context there. Only a native host has a context to pass, so such a
module is native only.

A `pub fn` marked `#[local]` is a method for native callers only, for values that exist only in
the host process, such as a closure or a type without serde. The WIT leaves it out, with the types
only it uses, which need no WIT spelling; the native side registers it with `local_method` under
the name it would have in WIT, so a local caller passes its arguments as for any method, or calls
it through the service's client, and a portable caller is refused with `LocalValue`. No other
method of the service, local or portable, may take that name, and a service without a `#[create]`
keeps at least one portable method for its interface. Such a module is native only too.

A component build refuses a native-only module: in a build for `wasm32`, each marker that makes it
native only, `#[inputs]`, `#[create]`, `#[root]`, an address guard, a method's `&Context` parameter
or `tembra::Result` result, and `#[local]`, is a compile error at the marker, all reported at once.
Compile such a module for native targets only, as above.

The codec reads and writes the JSON form compiled and TypeScript callers send and validate: the
arguments as an array, `null` too without parameters; a record as an object of its fields, keyed by
the lowerCamelCase form of their WIT names; an enum as the WIT name of its case; a variant and a
`Result` as `{"tag": .., "val": ..}`, without `val` for a case or a side that carries nothing, the
record of a case with named fields as an object; an option as `null` or the value; 64-bit integers
as exact numbers; a flags as an object of the flags set, each `true`, which reads a flag left out, or
`false`, as unset. A float crosses only when finite and not `-0`, and an `f32` only when it narrows
to such a number too: a value the codec refuses fails the call as its arguments, before the method
runs, and a result it cannot write fails it as its result. Free JSON, `serde_json::Value`, crosses
as a string that holds its text. Natively, a text that holds no JSON fails only that call, with the
reason `arguments`. A component has no error channel for it: its method panics and traps, as any
panic in a component method does, and the trap takes the revision down for every caller, each
later call and every other object failing as `unavailable`, until a reload installs the Orb again.
A newtype crosses as its field, read back through its `TryFrom`, which a `From` implements too: a
value it refuses fails only that call, with the error's message and the reason `arguments`. A
component reads a newtype through `From` instead, as it has no channel for that refusal but a trap,
so a newtype only a refusing `TryFrom` makes is native only: a component build of the module fails,
naming it. Each type of the module that enters the WIT, and each newtype, reads and writes its
JSON in its own module, through a hidden trait of the Orb module. A record, a variant, an enum or a
flags of an interface the module implements is the bindings' type, which implements the same trait
in the Orb module, written from the interface's WIT, wherever the WIT defines it: in that
interface, or in one it `use`s from, which the world imports.

With `context` on the attribute, a native host gets a `context` of its own, with the functions and
types of the component's: `principal()`, `cancelled()`, `cancel()`, `object()` and `log(..)` read
the call whose method is running, which the native side sets around each poll of the method's
future, so they answer after an `.await` and for each of several calls at once. The principal is
the one the call was admitted with, also once cancelled, and a record written with `log(..)` goes to
the observer of `Host::on_log`, attributed to the Orb by the host. A component build keeps the
bindings' `context`, through `tembra::__guest!`, the facade's counterpart of `tembra::__host!`.

These functions serve a method's body and the futures it awaits, nothing else. In a `#[create]`,
a `#[root]`, a task or a resource's closure, or a future spawned off the call, there is no call to
read: they panic there, and the host's executor does not catch the panic. A create reads its
object's address with `declaration.address()` instead, and a method reads what a future it spawns
needs before handing it over.

A `#[wit_resource]` struct is natively a value the Orb issues as a capability of its resource type,
as `tembra::portable::Issuable` names it: the resource's constructor and static functions, and a
function whose result holds the struct, alone or in an `Option`, a `Result`, a `Vec` or a tuple,
issue what they return into the call's result, and each method reads its `self` from the call. A
record or a variant holding the struct is component-only: the bindings replace it with a type
holding their handle, which only a component creates, so where the runtime is linked it is a
compile error. A tuple carries the resource beside its data in both builds:

```rust,ignore
impl Texts {
    /// A builder and the label it was started under: a tuple, where a record holding the builder
    /// would be component-only.
    pub fn labelled(label: String) -> (Builder, String) {
        (Builder::new(), label)
    }
}
```

The value belongs to the object that issued it, as a compiled exporter's belongs to its Store: it
closes, running its `Drop`, with its last owner, or when that object is reloaded or released or the
host closes. A method may be `async`: its call holds the value until it ends, through the guard of
`Receiver::guard`, so `&self` stays valid across its awaits, as in a component, also when the Orb is
reloaded meanwhile, after which the value closes once the call has ended. Natively a resource is
only returned, or borrowed as a method's `self`: a function taking one is a compile error where the
runtime is linked.

These functions have no local entrypoint, so the service's client calls them portably, through the
codec, and answers each resource as its handle type, `<Resource>Handle`, `BuilderHandle` for
`Builder`, public in the resource's module. A handle owns the resource's `Owned` handle, whose drop
drops the resource, and has one async method per method of the resource, which takes the call's
`&Context` first, as the client's do. The client offers the resource's constructor as
`new_<resource>` and each static function as `<resource>_<function>`, and a function whose result
holds the resource answers a handle in its place, alone or nested in a list, an option, a result or
a tuple, each handle naming its own value:

```rust,ignore
// In a native host, under a root context, with a reference to the `texts` object.
let texts = text::TextsClient::new(reference);
let builder: text::BuilderHandle = texts.new_builder(&context).await?;
assert_eq!(builder.push(&context, 'a').await?, 1);
let (labelled, label): (text::BuilderHandle, String) =
    texts.labelled(&context, "b".into()).await?;
assert_eq!((labelled.text(&context).await?, label), (String::new(), "b".into()));
drop(builder); // drops the resource: the Orb's value closes
```

Two methods of one name on a client, such as a function `new_builder` of the service beside the
constructor of `Builder`, are a compile error where the runtime is linked, naming both.
`BuilderHandle::try_from(owned)` wraps an `Owned` handle of the resource, such as one
`call_portable` delivers. An `Owned` handle of another resource type comes back as the error, still
open, so a caller holding a resource of an interface with several resource types can try the
handle type of each in turn; `Owned::from(handle)` gives the owned handle back, to move the
resource into a call's arguments. See [owned WIT resources](../../docs/rust.md#owned-wit-resources).

A [`#[state]`](#kernel-managed-state) serves a native host as it serves a component. The factory of
the service that owns it declares it with `Declaration::state`, from the state's `Default` at its
version, before the service's `#[create]` runs, and keeps it beside the object's environment; the
state's `read` and `update` reach the state of the object whose method is running, which the native
side sets around each poll of the method's future, as for the `context` functions. They serve the
methods of the owning service and the functions of its WIT resources only, which run on its
objects, and panic elsewhere, in a `#[create]` or a task for instance. The
kernel keeps the JSON it keeps for a component of the same module, the text `serde_json` writes,
with the Rust names of the fields and cases, so a host reads the state of either with
`reference.state::<serde_json::Value>()`. As in a component, a value the `#[validate]` function
refuses never reaches the kernel, and `update` returns the refusal; the kernel refuses the value of
a call cancelled since it started, and `update` returns the value it kept; and a reload reads the
value the new code inherits as the state's type and runs the `#[validate]` function on it before it
adopts it. An object declares one state, so a `#[create]` of the owning service declares none of
its own.

A [`#[resource]`](#resources-of-the-orbs-root) of the Orb's root serves a native host as it serves
a component. `orb(..)` gives each Orb it builds a table of its resources, which the Orb's create
fills, declaring each with `Declaration::resource` before the `#[root]`, if any, runs: the host
opens each with its `#[open]` function once the create returns, once for the Orb and before any
object of it is built, and closes it, through its `#[close]` function, when it removes the Orb. A
reload keeps it while its version stays the same, and replaces it at a new version, as
[above](#resources-of-the-orbs-root). The value lives in the host, one per Orb, so a crate
installed twice under two names has two of each.
`with` gives the open value to the methods of every service of the Orb and the functions of their
WIT resources, which the native side runs with their Orb's table around each poll of their future,
as for the `context` functions.
Elsewhere, in the root, a `#[create]` or a task, and before the host opens the resource or after
it closes it, `with` panics, as it traps a component. A root that fails fails the create, and no
resource opens.

An import of another Orb's interface, [plain](#importing-another-orbs-interface) or
[by key](#importing-another-orbs-interface-by-key), serves a native host when every function of the
interface is an `async func`. The struct `Providers`, generated beside `orb(..)`, names the Orb that
answers each import, a `tembra::Provider` of its name and API version, under the name of the
import's module, `gauge` or `gauge_ref`. `orb(..)` takes it last, `orb(providers)` or
`orb(inputs, providers)`, and so does `orb_named(..)`, so a host that leaves an import without a
provider does not compile:

```rust,ignore
let orb = metering::orb(metering::Providers {
    gauge: tembra::Provider::new("meters", 1),
    gauge_ref: tembra::Provider::new("meters", 1),
})?;
```

The Orb requires each provider, with the signature of the interface, as the plan of a compiled Orb
requires the provider an installation selects, so the host refuses the Orb without it. In place of
the bindings' import module, the Orb module names a module of the same name, with the interface's
types and functions, `async` as in a component and taking their parameters owned: each writes its
arguments as the boundary's JSON, calls the provider through the kernel with `call_portable`, under
the context of the method running, and reads the result back. A plain import reaches the provider's
`main` object, of the principal `host`, through a reference the Orb's create declares, as a
component's root does; the view of an import by key acquires the object of a key under the
principal of the call, whose root holds it. A call that fails, or whose arguments or result the
codec refuses, ends the method that made it with that error, where it traps a component. As the
`context` functions, the imported functions serve the methods and the contributions of the Orb and
the futures they await, and panic elsewhere. A native method has no fiber of its own to suspend
while the kernel completes a call, as a component's call has, so a synchronous imported function
cannot wait for its answer: an import of an interface with a synchronous function, plain or by key,
is a compile error where the runtime is linked, and works in a component build of the same module.

A [contribution to a hook](#contributing-to-a-hook) serves a native host too. `Providers` names the
Orb that declares each hook the module contributes to, under the name of the hook's module,
`guard`, and the Orb requires the hook of it, with the signature of its callback, as the plan of a
compiled contributor does. `orb(..)` adds each contribution with its priority, after the services:
the kernel runs it in the hook's plan, whether a native or a compiled Orb declares the hook, and
it reads the hook's input with the codec, runs the author's function and writes its answer back.
It runs in the Orb's root, as a component's contribution does: with the root's resources, its
imports and the call's `context`, whose `object()` is an empty key and the principal `host`, and
with no state. The runtime's contributions come with the facade's `hooks` feature, which a host
that installs such an Orb enables. A [hook declaration](#declaring-a-hook) is a compile error where
the runtime is linked: its `run` is synchronous, and natively a method has no fiber to suspend
while the kernel runs the hook's contributions, so the declaration works in a component build of
the module only.

The bindings of the kernel's `tembra:guest` package, which a module with a state, a root resource
or the `context` option imports, are a module named `tembra` inside the Orb module, which hides the
facade there: such a module names the facade `::tembra`, as in `::tembra::Result<Self>` and
`use ::tembra::Declaration;`.

Today the native side covers services whose functions take and return numbers, `bool`, `String`,
`char`, free JSON, records, enums, variants, flags, newtypes, `Result`s, and `Vec`, `Option` and
tuples of them, the types of an implemented or imported interface or of a hook among them, WIT
resources as above, states, the resources of the Orb's root, the imports of interfaces of
`async func`s and contributions to hooks. A type of a consumed interface that holds a resource, an
import of an interface with a resource and a WASI import are compile errors where the runtime is
linked, naming the item, until the native side covers them, and so is an `Option` of an `Option`,
which the boundary's JSON cannot tell from `None`; a component build of the same module is
unaffected.

## Moving a hand-written service to the macro

A native service written by hand with the runtime's builder, `Service::new(name, factory)` and its
`method(..)`s, keeps working as it is. Its names are the author's, often a service named like its
Orb and snake_case methods, and its JSON is whatever serde writes for its types. Moved to the
macro, as a `#[service]` struct with an `impl` block of `pub fn`, the service gets a WIT contract,
which compiled and TypeScript callers read, and its callers name, pass and receive other things:

- The service is named `<orb>::<interface identity>`: `<orb>` is the name the host installs the Orb
  under, from `[package.metadata.tembra]` or `orb_named(..)`, and the interface is the struct's
  name in kebab-case, in the Orb's package, unless `#[service("..")]` names the one it implements.
  `Plotter` of `example:calculator@1.0.0`, in an Orb installed as `calculator`, is
  `calculator::example:calculator/plotter@1.0.0`, the name a caller gives `Host::get` in place of
  the one the author chose.
- A method is named by its WIT name, the kebab-case form of its Rust name, for local callers too:
  `give_up` is `give-up`. The service's client keeps the Rust name, `give_up`.
- A local caller passes the arguments as a tuple of the parameters in WIT order, `()` without
  parameters, where each hand-written method chose its own argument type, such as `(T,)`, `()`,
  `[Value; 0]` or a struct of every argument. A portable caller sends them as a JSON array in WIT
  order, `[]` or `null` without parameters. A struct a method took whole is a record parameter,
  sent as `[{..}]`, unless its fields become the parameters.
- The fields of a record and the flags of a flags are keyed by the lowerCamelCase form of their WIT
  names, `dashedLine` for `dashed_line`, and an enum's cases are their WIT names, `north` for
  `North`, where serde writes the Rust names.
- A variant and a `Result` cross as `{"tag": .., "val": ..}`, without `val` for a case or a side
  that carries nothing, where serde writes an externally tagged `{"Rect": ..}` or `{"Ok": ..}`, or
  the tag of an internally tagged enum beside its fields. A case with named fields carries a record
  of its own, `<enum>-<case>`: the case `Rect { width, height }` of `Shape` carries a `shape-rect`.
- WIT has no generics, so a type that enters the WIT takes no type parameter. A record generic over
  what it holds, such as a page of any item, is written as a concrete record for each use, such as
  a page of notes whose items are a `Vec<Note>`; a type parameter reads as a type the module does
  not declare, a compile error.
- Free JSON, `serde_json::Value`, stays the author's type in the methods and crosses as a WIT
  `string` that holds its JSON text: a portable caller sends and receives the text, where the
  hand-written method took and gave the JSON itself. A local caller passes the value as before.
- A newtype, `struct Id(String)`, crosses as its field, as serde writes it, and is read back
  through its `TryFrom` natively and its `From` in a component: one that only a refusing `TryFrom`
  makes is native only, see [what the macro reads](#what-the-macro-reads).

The rest is as in [using the Orb natively](#using-the-orb-natively): a method whose error fails the
call returns `tembra::Result<T>`, an object's environment comes from a `#[create]`, and a method
that takes or returns host values is `#[local]`; each makes the module native only.

The native fixture's `calculator` shows the changes. Its plotter draws shapes:

```rust,ignore
#[tembra::orb(package = "example:calculator@1.0.0", context)]
pub mod calculator {
    pub enum Shape {
        Dot,
        Square(u32),
        Rect { width: u32, height: u32 },
    }

    #[service]
    pub struct Plotter;

    impl Plotter {
        /// The area of a shape, or why it has none.
        pub fn area(shape: Shape) -> Result<u64, String> {
            /* .. */
        }
    }
}
```

Its tests name, send and receive what the right column holds, where a hand-written service would
take and give what serde writes, in the left column:

| By hand, with serde                      | With the macro                                      |
| ---------------------------------------- | --------------------------------------------------- |
| a service `calculator`                   | `calculator::example:calculator/plotter@1.0.0`      |
| a method `give_up`                       | `give-up`                                           |
| `"North"`                                | `"north"`                                           |
| `{"Square": 4}`                          | `{"tag": "square", "val": 4}`                       |
| `{"Rect": {"width": 2, "height": 3}}`    | `{"tag": "rect", "val": {"width": 2, "height": 3}}` |
| `"Dot"`                                  | `{"tag": "dot"}`                                    |
| `{"Ok": 6}`                              | `{"tag": "ok", "val": 6}`                           |
| `{"filled": false, "dashed_line": true}` | `{"filled": false, "dashedLine": true}`             |
| `[1, 2]` as free JSON                    | `"[1,2]"`                                           |

```rust,ignore
// In a native host, under a root context.
let plotter = Rc::new(host.get(
    "calculator::example:calculator/plotter@1.0.0",
    vec!["main".into()],
    "host",
)?);
let rect = Shape::Rect { width: 2, height: 3 };
let area = plotter.call::<_, Result<u64, String>>(&context, "area", (rect,)).await?;
assert_eq!(area, Ok(6));
let rect = json!([{ "tag": "rect", "val": { "width": 2, "height": 3 } }]);
let area = plotter.call_portable::<_, Value>(&context, "area", rect).await?;
assert_eq!(area, json!({ "tag": "ok", "val": 6 }));
let client = calculator::PlotterClient::new(plotter);
assert_eq!(client.area(&context, Shape::Rect { width: 2, height: 2 }).await?, Ok(4));
```

## Items under `#[cfg]`

An attribute macro reads the module before the compiler evaluates the `#[cfg(..)]`s in it. On the
items the macro reads into the WIT and into `orb(..)`, a `#[service]` struct, an `impl` block of
one, a `pub fn` or the `#[create]` of such a block, the function marked `#[root]`, a function or a
struct marked `#[hook]`, and a sub-module that holds a service, the root or a hook, the macro
evaluates them itself: it expands the module once per combination of their distinct predicates.
Each copy of the module leaves out the items its combination rules out and goes through
`#[tembra::orb]` again under the `#[cfg]` of its combination, and the compiler keeps the one copy
whose combination holds. So a Cargo feature can add a service, a method, the Orb's root or a hook:
the WIT, the signatures, `orb(..)` and the contract the macro writes are those of the configuration
being built.

```rust,ignore
#[service]
pub struct Notes;

impl Notes {
    pub fn add(text: String) -> u32 {
        /* .. */
    }

    #[cfg(feature = "archive")]
    pub fn archive() -> u32 {
        /* .. */
    }
}

#[cfg(feature = "archive")]
#[service]
pub struct Shelf;

#[cfg(feature = "archive")]
impl Shelf {
    pub fn count() -> u32 {
        /* .. */
    }
}
```

A predicate counts once however often it is written, and `not(feature = "archive")` is the same
predicate as `feature = "archive"`, turned over. A module takes at most 4 distinct predicates, 16
copies; a 5th is a compile error at its `#[cfg]`. A copy left with nothing to export, every
service of the module under a `#[cfg]` that does not hold, is a compile error that names the
configuration of the build. The copies name the macro as `::tembra::orb`, through the facade the
crate depends on.

Any other `#[cfg]` is left to the compiler. On a field of the `#[inputs]` struct, which `orb(..)`
takes whole, or on a sub-module holding no service, root or hook, such as `#[cfg(test)] mod tests`,
it works as anywhere else; on another item the macro reads, such as a `#[state]`, a `#[resource]`
or a type that enters the WIT, it is a compile error, since the macro would read the item in every
configuration. A `#[cfg_attr(..)]` wrapping a `#[cfg(..)]` on an item of this section is a compile
error too: the macro evaluates the plain form alone.

The contract the macro writes follows the last build that compiled the crate for itself, with its
features: `cargo test --all-features` writes the contract of every feature, and a later
`cargo tembra build`, which compiles the component with the crate's default features, writes that
configuration's back. Commit the contract of the configuration consumers build against: for a
component, the one `cargo tembra build` writes, so run it last before committing `contract/`; for
a crate used only natively, the one a build with the default features writes.

## What it generates

The macro rewrites the module, or each copy of it under `#[cfg]`, see
[items under `#[cfg]`](#items-under-cfg), with:

1. `pub const WIT: &str`, the derived WIT text. When Cargo compiles the crate for itself, as the
   primary package, the macro also writes it to the crate's `contract/<world>.wit`, the consumed
   packages nested whole after it and the standard packages it depends on in `contract/deps/`:
   the files `cargo tembra build` writes, rewritten only when their content changes, so a crate
   used natively has the contract a compiled consumer reads, and a dependency's is never written;
2. the same text as a `tembra:wit` custom section of the Wasm module, and the manifest, the
   declared hooks with their composition, the contributions with their priorities, the
   services that chose a lifetime or a linger, the views of the interfaces imported by key with
   the interface each reaches, and the `wit` directory, as a `tembra:manifest`
   section. `cargo tembra build` reads them and packages the contract exactly as written here,
   with the `///` comments the binary type section loses, the hook imports the linker drops
   because nothing calls them, and every consumed package nested whole, not the part the
   component happens to call;
3. only with the `no_std` option, the freestanding Wasm prelude: `#[global_allocator]` on
   `dlmalloc` and a `#[panic_handler]` that traps, under `#[cfg(target_arch = "wasm32")]`; std
   guests keep their standard runtime, and host checks require no Wasm target. When the macro
   refuses the module of a `no_std` guest, the prelude comes with its error, which is then the
   only one the compiler reports;
4. `generate!({ inline: WIT, world, generate_all, runtime_path })`, the official `wit-bindgen`
   macro, called through the facade's re-export of the version whose rules the macro follows,
   with the bindings' runtime reached there too, and the WIT
   as a string, plus `path` when the Orb consumes other packages and, with a state, a resource or
   the context, the kernel's `tembra:guest` package nested in the inline text, unless the consumed
   directory holds it already, as a contract `cargo tembra build` wrote does in its `deps/`; that
   copy, as every copy of a bundled package the directory holds, used or not, must be a file of
   `deps/` itself, the bundled file line for line, whatever its line endings, or, for a WASI
   package, an encoded package the tools merge with it; the macro names any other, and asks to
   remove a copy of the kernel's package an encoded package holds, since it brings that package
   itself. With a WASI import, the WASI packages it needs are nested the same way, from the
   sources `tembra-contracts` bundles.
   With a state, also `serde` derives on the generated types, except the records and
   variants holding a consumed package's flags type or a resource handle, which have no `serde`
   implementation to derive. `wit-bindgen` leaves those out by their bare names, so a type
   sharing the name of one, in another interface or package, takes the derives by its full name
   in `additional_type_attributes`. With derives on the types that enter the WIT, also `additional_type_attributes` and
   `additional_member_attributes`, which hand them to the generated types with the helper
   attributes they read. With a `#[flags]` struct, also `with`, which maps each flags type onto
   the author's struct;
5. `pub use exports::...::{Stats}`: the author's record and enum definitions are replaced by the
   generated types with the same names, public fields and derives, so the author's functions and
   the generated traits share one type, and each derive of such a type is imported again under no
   name, `use EnumIter as _;`, where the type was written, so the imports it names stay used;
   `pub use exports::...::guard;` for each contributed hook and each
   implemented interface, `pub use ...::guard;`, the import module, for each hook declared from a consumed package and each imported interface; `pub use ...::counter_ref;`, the import module of the view, for each interface imported by key; `pub use ...::context;` with the `context` option, inside `tembra::__guest!`, so only in a component build;
6. the author's items, unchanged, but for each `#[flags]` struct that enters the WIT the derives
   it lacks, `Clone`, `Copy`, `Debug` and, with a state, serde's, and the functions its bindings
   call on it, `empty`, `bits`, `from_bits_retain` and `BitOr`;
7. a private struct implementing the `Guest` trait of every interface, derived or implemented,
   but that of a service its `#[create]` builds, by forwarding to the author's functions, hook
   callbacks and the kernel's `state-init` and
   `resources-init` callbacks included, with a trapping callback for each declared hook, and
   `export!` under `#[cfg(target_arch = "wasm32")]`, so that a native build defines none of the
   component's export symbols; the resource trait of each `#[wit_resource]` struct on the struct
   itself, its constructor and methods forwarded and the handles wrapped, or taken back, at the boundary; with a declared hook, `run` on its struct, calling the hook's import, and, for a hook derived from `input = Type`, `From` in both directions between the two sides of the bindings for the types it reaches;
8. with a state, `Default`, field by field unless the author provides it, and the `read`/`update`
   functions on each state type, over a hidden module that maps the kernel's JSON to the type with
   `serde_json` and holds back a value whose JSON would not read back as the type, and reaches the
   kernel through the bindings' import in a component build, inside `tembra::__guest!`, and through
   the state each method's object declared natively, inside `tembra::__host!`; and the kernel's
   `validate` callback, which reads the value a reload would adopt as the state type and runs the
   state's `#[validate]` function on it;
9. with a resource, one slot in the component's memory per resource, filled by the `#[open]`
   function and emptied by the `#[close]` one, and the `with` function on each resource type,
   which reaches the slot in a component build, inside `tembra::__guest!`, and natively, inside
   `tembra::__host!`, the resource of the Orb whose method runs, from a hidden table of the Orb's
   resources that the Orb's create fills;
10. inside `tembra::__host!`, so only where the facade links the runtime, `pub fn orb()`, or
    `orb(inputs)` with an `#[inputs]` struct, which builds the Orb named by the manifest through
    `pub fn orb_named(name, api_version, ..)`, whose Orb's create declares the root's resources and
    runs its `#[root]`, from one
    hidden function per service, written in the service's module and declaring the codec of each
    of its methods, and the state of its objects, if they own one, next to the service's typed
    client, `<Service>Client`, the handle type of each WIT resource, `<Resource>Handle`, in the
    resource's module, an `include_bytes!` of the manifest, and a hidden trait that each
    type of the module entering the WIT implements in its own module, reading and writing its JSON;
    with the `context` option, the native `context`, whose functions read the call each method's
    future runs for, see [using the Orb natively](#using-the-orb-natively).

The crate must list `dlmalloc` (feature `global`) among its dependencies only with `no_std`, and
`serde` (features `derive`, `alloc`) and `serde_json` (feature `alloc`) when it has a state: the
generated code names them. `wit-bindgen` it does not list: the facade's `macros` feature brings
the version whose rules the macro follows, and the generated code reaches it there.

## Limits

- WASI interfaces are imported, never implemented: serving incoming HTTP, an export of
  `wasi:http/handler`, is not expressed, and a WASI interface's types and handles stay out of a
  derived signature, also where another Orb's interface `use`s them. An Orb consumes another
  Orb's WIT by importing an interface, plainly or by key, implementing one or contributing to a
  hook, and passes the interface's data types on as they are. A view takes the name of the
  interface imported by key, so two interfaces of one name are not both imported by key.
- A hook declared from a consumed package keeps the types of its import, which `run` takes and returns, while a derived interface takes the types of its export: a method that reports its record returns a record of the module, converted from it. A hook derived from `input = Type` shares the module's types, defined in the hook's interface; its input is a record, an enum, a variant, a scalar, a string or a list, of the module's own types. `run` hands the import a string, a list, or a record or variant holding one by reference, as the bindings take them, and lends an option, a result or a tuple over them piece by piece, as the bindings borrow it, copying its other parts.
- A service's functions are associated functions without `self`, unless the service's
  `#[create]` builds its objects, natively; the methods of a `#[wit_resource]` struct take
  `&self`, and none takes `&mut self`. Besides a service's state, a resource's fields, the root's
  resources and the fields of a service its `#[create]` builds, data lives in `static`s, as in
  the compiled fixtures of `executor-wasm`.
- Root resources, `#[resource]`, belong to the root, declare no dependencies on one another, and
  are opened in the order the kernel chooses; the `#[open]` and `#[close]` functions take no
  arguments besides the value.
- A state is serialized by `serde`: the kernel sees the Rust field names and enum cases, not the
  camelCase spelling the executor gives WIT values on the portable boundary, a consumed type's
  included. The macro derives `Serialize` and `Deserialize` on a state that stays out of the WIT,
  and on every type of the module its fields reach that stays out too, unless the author already
  does, before the compiler evaluates `#[cfg_attr(..)]`: a derive of either under that attribute
  is a compile error on it, since the type would implement the trait twice where it applies, so
  derive it without the attribute, or leave it to the macro. Such a derive is `serde`'s when the
  macro follows it to `serde`, and when it cannot follow it to a crate, through a glob import or
  `super::`, as on a type that enters the WIT; another crate's, `rkyv::Serialize`, is that
  crate's own. JSON holds no NaN or infinite float, so a state holding one cannot be stored, and
  the JavaScript hosts (Node, mobile, browser) parse JSON numbers as doubles, so an integer beyond
  ±2^53 in a state loses precision there.
- Types: records, enums, variants, flags and the scalar, string, list, option, result and tuple
  types; 64-bit integers cross the boundary exactly. A flags is a `#[flags]` struct of one to 32
  `bool` fields; its functions `empty`, `bits` and `from_bits_retain` and its `BitOr` are the
  macro's, for its bindings, and a derived hook's input holds none. A consumed interface's data
  type, named through its module, crosses as the consumed package's own type, see
  [the types of a consumed interface](#the-types-of-a-consumed-interface). A `resource` is a
  `#[wit_resource]` struct or the handle of an imported interface. A derived function takes either
  as an owned parameter, a struct only in the interface that defines it, and returns only a struct
  of its own interface; an implemented interface takes the owned handles its WIT says. A borrowed
  parameter, `borrow<t>`, is not derived: the runtime refuses it at installation today ("borrow
  requires a dedicated interpreter value adapter"). No `stream` or `future`. `()` is nothing only
  as a function's result or a side of a `Result`, which WIT then leaves out: `Result<T, ()>` is
  `result<t>`, `Result<(), E>` is `result<_, e>` and `Result<(), ()>` is `result`; `Option<()>` has
  no equivalent, use `bool`. `Option<Option<T>>` is refused, an option of an option, which the
  runtime refuses in a signature: use an enum of the module with a case for each.
- An implemented interface is fixed by its WIT: the `impl` adds no function to it. A state is a
  struct of the module, which holds a consumed package's records as they are but is none of them.
- A type that enters the WIT is replaced by the type the bindings generate, which takes the
  author's `#[derive(..)]`s and the helper attributes they read: the single-name attributes on the
  type, its fields and its cases, such as `#[default]` or `#[strum(..)]`, but none of Rust's own
  attributes, such as `#[allow(..)]` or `#[repr(..)]`. A derive of what the bindings implement
  themselves is left to them: `Debug` for every type; `Clone` for a record or a variant that holds
  no resource handle, and `Copy` when it holds no string or list either; `Clone`, `Copy`,
  `PartialEq`, `Eq`, `PartialOrd` and `Ord` for an enum; `Display` and `Error` for the error of a
  function's result; and `serde`'s derives in an Orb with a state. A derive of another crate named
  after one of those traits, or implementing one under another name, `Educe` or `Derivative` with
  the trait in `#[educe(..)]` or `#[derivative(..)]`, would implement it twice, and a derive that
  the type's contents rule out could not apply, which the macro decides trait by trait from the
  fields: `Clone`, `Copy`, a comparison, `Hash`, `Serialize` or `Deserialize` on a type holding a
  resource handle, `Serialize` or `Deserialize` on one holding a consumed package's flags type,
  alone or in a consumed type, or, in an Orb without a state, where the bindings derive serde's
  traits on no type, any type of a consumed package, `Default` on a record no value of which is
  without one, a handle in a field, in a tuple or in such a record, `Copy` on one holding a string
  or a list, `Eq`, `Ord` or `Hash` on one holding an `f32` or an `f64`, and `Default` on a record
  holding a `Result`. A handle, a string or a list held through a type of the module or a consumed
  type counts the same, unless the macro sees the trait implemented for that type: by the bindings,
  by its own derives, by an `impl` of the Orb's modules, by the macro for a state's `Default` or a
  flags struct's derives. A trait such a type lacks otherwise, `PartialEq` on a record holding a
  `Spot` that derives none, or holding a consumed record, which its bindings give `Debug`, `Clone`
  and `Copy` only, as [the types of a consumed interface](#the-types-of-a-consumed-interface)
  says, is left to the author, who may implement it for the type by hand anywhere in the crate,
  outside the Orb module or through a `crate::` path, where the macro does not read; a missing one
  is the compiler's error inside the bindings. Both refusals are compile errors on the derive, the
  first naming the helper attributes no other derive of the type reads,
  which go with it, `#[error(..)]` for `thiserror::Error` on a function's error, or the entry of
  `#[educe(..)]` to remove, the second naming what the type holds. A handle in an option or a list
  leaves `Default` a value without it, as does any type held there, and a variant takes its unit
  `#[default]` case. A derive implementing one of those traits under another name follows the same
  rules, but for a member carrying the helper attribute the macro knows the derive reads,
  `#[default(..)]` of `SmartDefault` or `#[educe(..)]` of `Educe`, which may give the member its
  own value or leave it out: what that member holds, a handle included, is the derive's.
- The bindings read a derive's path in their own module, so the macro names each derive from its
  crate, `::strum::EnumIter` for `EnumIter` after `use strum::EnumIter;`, following the `use` and
  `extern crate` declarations of the type's module, `self::`, `super::`, the Orb's sub-modules and
  the glob imports from them, `use super::*;` in a sub-module; a derive of the standard library is
  named from `core`. A single name the module does not import by name, other than a derive of the
  standard library, which only a crate's glob import could bring, or a path leaving the Orb module
  through `super::`, is a compile error saying what to write instead. `serde`'s derives are the
  exception where the bindings derive them, in an Orb with a state: a `Serialize` or `Deserialize`
  the macro cannot follow to a crate, through such a glob, through `super::` or through a re-export
  of this crate, `crate::prelude::Serialize`, is left to the bindings as `serde`'s own is, and
  refused before its path on a type whose contents rule it out. Another crate's derive of either
  name, `rkyv::Serialize`, is that crate's own and carried as any other, so a derive of serde's
  that another crate re-exports is written `serde::Serialize`, or through `crate::`.
- The bindings' module reads the helper attributes too, as written, so one it would not know is a
  compile error on the attribute: one no derive the generated type takes reads, since a derive of
  the standard library reads none but `#[default]` and a derive the bindings implement is theirs,
  as `Debug` is after `use derive_more::*;`; an attribute macro the type's module imports,
  `#[skip_serializing_none]`, which is no derive's helper, unless a derive the generated type takes
  reads an attribute of that name, since a helper attribute resolves in the macro namespace, where
  a module or a type the module imports, `use core::default;` under `#[default]`, shadows nothing;
  and arguments naming what the type's
  module declares or imports, or a path from `self::` or `super::`, which a path from `crate::`,
  `#[default(crate::walk::PAGE)]`, or from a crate names from every module. A type of the module
  that enters the WIT is the exception: the bindings' module declares the generated type replacing
  it under the same name, where the type's interface defines or `use`s it, so `#[default(Mode::Slow)]`
  on a field holding the module's `Mode` reads there as written.
- What the replacement would drop without a word is refused on a type that enters the WIT: an
  explicit discriminant, which a WIT enum has no room for, a `#[serde(..)]` attribute, which would
  change a state's JSON, and a helper attribute on the payload of a case, which the bindings have
  no place for.
- An enum with a case with named fields, `Refused { reason: String }`, is not replaced, nor is any
  type holding it, directly or through other types of the module. The case enters the WIT as a
  record of its own, `<enum>-<case>`, `outcome-refused`, which the bindings give the case as a tuple
  payload, so the author's type stays as written, attributes and derives included, and the macro
  converts it to and from the bindings' type at the boundary, `From` in both directions in the
  type's module, inside lists, options, results and tuples too. Such a type derives `serde`'s traits
  where a state reaches it, unless the author does. The bindings name its fields and cases from their
  WIT names, which the conversions use on their side, so the author's names are free. It holds no
  resource, and a derived hook's input reaches none of them: both are compile errors, as is another
  type or a function of the interface whose WIT name is the record's.
- The macro reads a type that enters the WIT, a flags struct included, before the compiler
  evaluates `#[cfg(..)]` and `#[cfg_attr(..)]`. The type, a field, a case or a case's payload under
  `#[cfg(..)]`, which the WIT would hold in every configuration, is a compile error, and so is a
  `#[cfg_attr(..)]` wrapping a derive, a helper attribute or a `#[cfg(..)]`, which would come and
  go beside what the macro writes, on a flags struct or a type the generated type replaces; a type
  the macro converts keeps its attributes. One wrapping only Rust's own attributes other than
  `derive` and `cfg`, such as `#[cfg_attr(test, allow(dead_code))]`, `doc` or `deprecated`, changes
  nothing the macro writes.
- The macro writes the code serving a struct marked `#[state]`, `#[resource]` or `#[wit_resource]`,
  and the code calling the `pub fn` of a WIT resource and a function marked `#[constructor]`,
  `#[validate]`, `#[open]` or `#[close]`, before the compiler evaluates `#[cfg(..)]`, and that code
  names the struct or the function in every configuration: such a struct, such a function, the
  `impl` block holding it or a sub-module holding either under `#[cfg(..)]`, written plainly or
  under `#[cfg_attr(..)]`, is a compile error. A `#[service]` struct, its `impl` blocks and their
  `pub fn` and `#[create]`, the `#[root]`, a `#[hook(..)]` item and a sub-module holding one of
  these take a plain `#[cfg(..)]`, which the macro evaluates itself, see
  [items under `#[cfg]`](#items-under-cfg); one under `#[cfg_attr(..)]` there, which the macro
  cannot evaluate, is a compile error that points to the plain form.
- Keep no `wit/` directory in the crate: `wit-bindgen` loads it on its own and finds the package
  twice.

## Async implementations of existing WIT

An implemented interface keeps the calling convention written in its WIT. Implement an
`async func` with `pub async fn`, including resource methods and static factories. The macro
forwards the future with `.await` through the official `wit-bindgen` async bindings; it does
not run an executor or block the guest thread. Those bindings use `wit-bindgen`'s `async` feature,
which the facade enables.

For example, given an existing interface:

```wit
interface feed {
  resource updates {
    next: async func() -> option<u64>;
  }
  subscribe: async func() -> updates;
}
```

```rust,ignore
#[tembra::orb(package = "example:consumer@1.0.0", wit = "wit")]
mod events {
    #[service("example:events/feed@1.0.0")]
    pub struct Feed;

    impl Feed {
        pub async fn subscribe() -> Updates {
            Updates
        }
    }

    #[wit_resource(service = Feed)]
    pub struct Updates;

    impl Updates {
        pub async fn next(&self) -> Option<u64> {
            Some(1)
        }
    }
}
```

Async functions can await asynchronous imports, and a resource's `&self` borrow remains valid
while its method is suspended. A function's async or sync declaration must match the existing
WIT; a mismatch is a macro error. Constructors remain synchronous; use an async static factory
when construction needs to await work. Rust-first declarations also retain `async` in the WIT
they derive.

`just test-orb-macro` builds real guests and exercises host-controlled suspension and resumption
of an interface factory, a resource method and a static factory on both execution engines.
