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
wit-bindgen = { version = "=0.63.0", default-features = false, features = ["macros", "realloc"] }
```

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

| In the module                                                               | In the WIT                                                                                                                                                         |
| --------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| the module name                                                             | the `world`                                                                                                                                                        |
| `package = "..."` on the attribute                                          | `package ns:name@version;`                                                                                                                                         |
| `wit = "dir"` on the attribute                                              | the directory with the WIT packages the Orb consumes, relative to the crate                                                                                        |
| `context` on the attribute                                                  | `import tembra:guest/context` in the world: the call's principal and cancellation, the object's address, the log                                                   |
| a unit struct with `#[service]` and an `impl` block of `pub fn`             | an exported `interface`, in kebab-case                                                                                                                             |
| `#[service("ns:name/interface@version")]` on a unit struct                  | `export` of that interface of a consumed package, implemented by the `impl` block                                                                                  |
| `#[service(lifetime = host)]` or `#[service(linger_ms = n)]`                | nothing in the WIT: the ownership policy of the service's objects, in the manifest                                                                                 |
| `import = "ns:name/interface@version"` on the attribute                     | `import` of that interface of a consumed package, bound to a provider at installation                                                                              |
| `import = "wasi:name/interface@0.3.0"` on the attribute                     | `import` of that interface of a WASI package bundled with `tembra-contracts`, granted at installation                                                              |
| `import("ns:name/interface@version", keyed)` on the attribute               | `import` of a view of that interface, `interface-ref` in the package: a resource with the interface's functions as methods, and `get`, selecting an object by key  |
| a struct with `#[wit_resource]` and an `impl` block of `pub fn`             | a `resource` of its service's interface: the `#[constructor]` function its constructor, the `&self` functions its methods                                          |
| a `#[wit_resource]` struct or an imported `iface::Name` as a parameter      | an owned handle, `own<t>`, which the call takes from its caller; an imported interface's resource is `use`d from it                                                |
| each `pub fn` without `self`                                                | a `func`; name and parameters in kebab-case                                                                                                                        |
| a struct with named fields, when a service method uses it                   | a `record`                                                                                                                                                         |
| an enum, when a service method uses it                                      | an `enum` when every variant is empty, otherwise a `variant`                                                                                                       |
| a struct of `bool` fields with `#[flags]`, when a service method uses it    | a `flags`, a flag per field in kebab-case; the struct stays the author's, which the bindings take as it is                                                         |
| `i32`, `u32`, `String`, `Vec<T>`, `Option<T>`, `Result<T, E>`, tuples       | `s32`, `u32`, `string`, `list<T>`, `option<T>`, `result<T, E>`, `tuple<...>`                                                                                       |
| a free `pub fn` with `#[hook("ns:name/hook@version", priority = n)]`        | `import` and `export` of that hook interface in the world: a contribution                                                                                          |
| a unit struct with `#[hook("ns:name/hook@version", composition = c)]`       | `import` and `export` of that hook interface in the world: a declaration                                                                                           |
| a unit struct with `#[hook(composition = c, input = Type)]`                 | a hook `interface` of the package under the struct's name, its callback `apply` and the types the input reaches, imported and exported by the world: a declaration |
| a struct with named fields and `#[state(service = Name, version = n)]`      | `import tembra:guest/state` and `export tembra:guest/state-init` in the world, and a `record` when a method uses it                                                |
| a struct with `#[resource(version = n)]` and a function marked `#[open]`    | `import tembra:guest/resources` and `export tembra:guest/resources-init` in the world                                                                              |
| `#[validate]` on a `fn(&self) -> Result<(), String>` of a `#[state]` struct | nothing in the WIT: the state's rule, run on every update and on reload                                                                                            |
| `#[derive(..)]` on a struct or an enum that enters the WIT                  | nothing in the WIT: the generated type replacing it derives the same, besides what the bindings implement themselves                                               |
| `#[close]` on a `fn(self)` of a `#[resource]` struct                        | nothing in the WIT: run by the kernel at teardown                                                                                                                  |
| an inline `mod`                                                             | nothing of its own: its items are read as the Orb's, grouped with what belongs to them                                                                             |
| `///` comments and `/** */` blocks                                          | `///` comments in the WIT, one per line                                                                                                                            |

Nothing is exported by accident. A service is a unit struct marked `#[service]`; the `pub fn` of its
`impl` blocks are its methods, private functions stay private. A struct or enum enters the WIT only
when a service method, a WIT resource's function or a hook's input uses it, directly or through the
fields of another type that enters; a state alone does not make a type enter. The other types of the
module, and unit structs without the marker, remain ordinary Rust, whatever they hold: only a type
that enters the WIT needs a WIT spelling for its fields and cases, so an array field or a variant
with named fields is a compile error there and nowhere else. A state that stays out of the WIT, and
every type of the module its fields reach that stays out too, get `serde`'s derives. A method that
names a type the module does not declare is a compile error, and so is a service with the name of
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
artifact.

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
fits the composition. Which role an interface plays travels in the `tembra:manifest` section next
to the contribution priorities, and `cargo tembra build` packages it; an Orb declares a hook or
contributes to it, never both.

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
`#[derive(Default)]` on the state, which stays on the author's struct when the state stays out of
the WIT and goes to the generated type replacing it when the state enters the WIT, as every derive
of a type that enters it does (see [Limits](#limits)). That is how a state starts from a value of
the author's choosing:

```rust
impl Default for Count {
    fn default() -> Self {
        Count { total: 100, steps: 0 }
    }
}
```

The macro's `Default`, like a derived one, gives each field its type's `Default`. A field whose type
has none, an enum of the module for instance, is a compile error on that field: implement `Default`
for the state, as above, or give the field's type one, by hand or by a derive, which for an enum
marks the case it starts from:

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
grants nothing for them.

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
resource struct enters the WIT only when a service method uses it; a resource may hold types the
WIT has no spelling for, such as a client of some granted I/O.

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
or compiled, to one contract, and an installer binds a consumer of the interface to either. See
[rust-orb-shared-interface](../../examples/rust-orb-shared-interface/README.md).

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
kernel checks the provider against the same signature. See
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
name are `use`d from the interface, so they stay the provider's. The view's generated module is
re-exported under the view's name: `counter_ref::get(key)` acquires the object of the key, the
`Vec<String>` of its parts, under the principal of the call, and returns a `counter_ref::Counter`
whose methods call that object. A key the kernel refuses, one the provider does not admit for
instance, is the `Err` of `get`, and the call goes on. One key reaches one object, through any
number of handles; another key or another principal reaches another object. `get` is
asynchronous: the function calling it is a `pub async fn`, `async` in the WIT as well, and the
crate enables the `async` feature of `wit-bindgen`. A handle belongs to the call's root: dropping
it releases its claim when the call settles, and the root's end expires every claim, a handle
kept in a `static` included, so a later call cannot use it.

The manifest's `references` map the view to the interface it reaches, and `cargo tembra build`
packages that role in the artifact; the kernel checks the view's methods against the provider's
functions when the Orb is installed. The installer binds the view by its own identity, as it
binds a plain import:

```rust
let install = Install::default().requires("example:router/counter-ref@1.0.0", "counters", 1);
let router = engine.load(&artifact, install)?;
```

The objects of a view carry data: an interface with a resource, or a function taking or
returning a resource handle, a stream or a future, is not imported by key. A plain import of the
same interface may stand next to the keyed one, re-exported under the interface's name. The
`counting` service of the [runtime fixture](runtime-fixture/src/lib.rs) imports by key.

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
Rust refuses.

The WASI packages are bundled with `tembra-contracts`, so a WASI import needs no `wit = "dir"`:
the macro reads the interface from its bundled package and nests that package, with the bundled
packages it builds on, in the WIT the bindings read. A consumed directory may hold some of them
already, as a contract `cargo tembra build` wrote holds those it depends on in its `deps/`: the
bindings read those from there, and each must define what the bundled package defines, whatever
its comments, or the macro names its file. The `wasi` namespace holds the bundled packages and
nothing else: an identity naming none of their interfaces, of another version or unstable like
`wasi:clocks/timezone`, is a compile error listing what there is.

WASI 0.3 is asynchronous where it waits: a clock's waits, files, sockets and HTTP are `async func`,
whose bindings are `async fn` a method awaits, as `throw_after` does, and standard input and output
are streams and futures. The bindings of such an interface use `wit-bindgen`'s async support, so
the guest enables its `async` feature, as
[async implementations](#async-implementations-of-existing-wit) do, even when its methods call only
the interface's synchronous functions, such as `monotonic_clock::now()`; `wasi:random` needs none.

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
configured provider. An interface the host does not serve as an import, such as `wasi:cli/run`,
which a command exports, compiles but does not install.

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
resources' functions, and the others `use` it from there; interfaces that would `use` each other,
which WIT refuses, are a compile error naming them. See
[rust-orb-wit-resource](../../examples/rust-orb-wit-resource/README.md).

## Taking a resource

A derived function takes a resource as a parameter, owned: a `#[wit_resource]` struct of its own
interface, `T`, which the WIT names `own<t>`, or the handle of an imported interface,
`texts::Builder`, named through the interface's module, which the derived interface `use`s from
the imported one. Either may sit in an `Option`, a `Result`, a `Vec` or a tuple, and a service's
functions, a resource's constructor, its methods and its static functions take them alike:

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
Rust name, so a type or a resource of the module named like the handle is a compile error. A
record or a variant holding a resource is the bindings' type, with the bindings' handle in the
resource's place: a function taking one gets the handle, whose `into_inner` gives the value.

The runtime moves an owned resource only into the interface that defines it, or into an Orb that
imports that interface. It refuses at installation today an Orb whose function takes a borrowed
resource, `borrow<t>` ("borrow requires a dedicated interpreter value adapter"), so the macro
derives none: `&T` is refused as every reference in a signature is. A function taking a resource of
another service's interface or of an implemented interface, itself or in a record or a variant of
the module, a WASI handle, which the runtime moves between no Orbs, and a hook's input holding a
resource are compile errors too; so is a derived function returning an imported interface's handle,
or another service's resource, which the runtime returns only from the interface that defines it.

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
names are the bindings', so a function of one of them in the struct's own `impl` blocks, or the
author's own `BitOr`, is a compile error. The macro also derives `Clone`, `Copy` and `Debug`, which
a generated record or variant holding the struct needs of it, and, in an Orb with a state, serde's
`Serialize` and `Deserialize`, so a state can hold the struct, directly or in a record; each unless
the author derives or implements it. A state's JSON holds every field of the struct,
`{"read": true, "write": false, "share": false}`, where the portable boundary spells a flags value
as an object of its set flags in camelCase, `{"read": true}`, a flag left out being unset.

A flags holds one to 32 flags, as the component model rules, and its struct takes no generic
parameter; a flags struct of a sub-module is `pub` or `pub(super)`, visible to the Orb module, as
the bindings name it from there. A derived hook's input holds no flags: the bindings name a hook's
types from its import module and from its export module, at two depths, and the macro maps a flags
onto the author's struct through one path. A struct marked `#[flags]` that no function uses stays
ordinary Rust.

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
resource's `#[open]` and `#[close]` functions, is `pub` or `pub(super)` in a sub-module. A name that
enters the WIT is one across the modules, because the package has one namespace: the services, the
hooks, the resources and the types a method or a hook mentions; a state that stays out of the WIT
may repeat its name module by module, as `Balance` does, and a module's own name wins when a method
mentions it. The modules the macro re-exports in the Orb module, `context`, an imported interface, a
hook declared from a consumed package, are reached from a sub-module through `super::`. See
[rust-orb-service-lifetime](../../examples/rust-orb-service-lifetime/README.md).

## What it generates

The macro rewrites the module with:

1. `pub const WIT: &str`, the derived WIT text;
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
4. `wit_bindgen::generate!({ inline: WIT, world, generate_all })`, the official macro with the WIT
   as a string, plus `path` when the Orb consumes other packages and, with a state, a resource or
   the context, the kernel's `tembra:guest` package nested in the inline text, unless the consumed
   directory holds it already, as a contract `cargo tembra build` wrote does in its `deps/`; that
   copy must define what the bundled package defines, whatever its comments, or the macro names
   its file. With a WASI import, the WASI packages it needs are nested the same way, from the
   sources `tembra-contracts` bundles.
   With a state, also `serde` derives on the generated types, except the records and
   variants holding a consumed package's flags type or a resource handle, which have no `serde`
   implementation to derive. With derives on the types that enter the WIT, also `additional_type_attributes` and
   `additional_member_attributes`, which hand them to the generated types with the helper
   attributes they read. With a `#[flags]` struct, also `with`, which maps each flags type onto
   the author's struct;
5. `pub use exports::...::{Stats}`: the author's record and enum definitions are replaced by the
   generated types with the same names, public fields and derives, so the author's functions and
   the generated traits share one type, and each derive of such a type is imported again under no
   name, `use EnumIter as _;`, where the type was written, so the imports it names stay used;
   `pub use exports::...::guard;` for each contributed hook and each
   implemented interface, `pub use ...::guard;`, the import module, for each hook declared from a consumed package and each imported interface; `pub use ...::counter_ref;`, the import module of the view, for each interface imported by key; `pub use ...::context;` with the `context` option;
6. the author's items, unchanged, but for each `#[flags]` struct that enters the WIT the derives
   it lacks, `Clone`, `Copy`, `Debug` and, with a state, serde's, and the functions its bindings
   call on it, `empty`, `bits`, `from_bits_retain` and `BitOr`;
7. a private struct implementing the `Guest` trait of every interface, derived or implemented,
   by forwarding to the author's functions, hook callbacks and the kernel's `state-init` and
   `resources-init` callbacks included, with a trapping callback for each declared hook, and
   `export!`; the resource trait of each `#[wit_resource]` struct on the struct itself, its
   constructor and methods forwarded and the handles wrapped, or taken back, at the boundary; with a declared hook, `run` on its struct, calling the hook's import, and, for a hook derived from `input = Type`, `From` in both directions between the two sides of the bindings for the types it reaches;
8. with a state, `Default`, field by field unless the author provides it, and the `read`/`update`
   functions on each state type, over a hidden module that maps the kernel's JSON to the type with
   `serde_json` and holds back a value whose JSON would not read back as the type, and the
   kernel's `validate` callback, which reads the value a reload would adopt as the state type and
   runs the state's `#[validate]` function on it;
9. with a resource, one slot in the component's memory per resource, filled by the `#[open]`
   function and emptied by the `#[close]` one, and the `with` function on each resource type.

The crate must list `wit-bindgen` (features `macros` and `realloc`, and `async` with an import by
key) among its dependencies,
`dlmalloc` (feature `global`) only with `no_std`, and `serde` (features `derive`, `alloc`) and `serde_json`
(feature `alloc`) when it has a state: the generated code names them.

## Limits

- WASI interfaces are imported, never implemented: serving incoming HTTP, an export of
  `wasi:http/handler`, is not expressed. An Orb consumes another Orb's WIT by importing an
  interface, plainly or by key, implementing one or contributing to a hook. A view takes the name
  of the interface imported by key, so two interfaces of one name are not both imported by key.
- A hook declared from a consumed package keeps that package's types: a method that reports its record returns a record of the module, converted from it. A hook derived from `input = Type` shares the module's types, defined in the hook's interface; its input is a record, an enum, a variant, a scalar, a string or a list. `run` hands the import a string, a list, or a record or variant holding one by reference, as the bindings take them, and lends an option, a result or a tuple over them piece by piece, as the bindings borrow it, copying its other parts.
- A service's functions are associated functions without `self`; only the methods of a
  `#[wit_resource]` struct take `&self`, never `&mut self`. Besides a service's state, a
  resource's fields and the root's resources, data lives in `static`s, as in the compiled
  fixtures of `executor-wasm`.
- Root resources, `#[resource]`, belong to the root, declare no dependencies on one another, and
  are opened in the order the kernel chooses; the `#[open]` and `#[close]` functions take no
  arguments besides the value.
- A state is serialized by `serde`: the kernel sees the Rust field names and enum cases, not the
  camelCase spelling the executor gives WIT values on the portable boundary. The macro derives
  `Serialize` and `Deserialize` on a state that stays out of the WIT, unless the author already
  does. JSON holds no NaN or infinite float, so a state holding one cannot be stored, and the
  JavaScript hosts (Node, mobile, browser) parse JSON numbers as doubles, so an integer beyond
  ±2^53 in a state loses precision there.
- Types: records, enums, variants, flags and the scalar, string, list, option, result and tuple
  types; 64-bit integers cross the boundary exactly. A flags is a `#[flags]` struct of one to 32
  `bool` fields; its functions `empty`, `bits` and `from_bits_retain` and its `BitOr` are the
  macro's, for its bindings, and a derived hook's input holds none. A `resource` is a `#[wit_resource]` struct or the
  handle of an imported interface. A derived function takes either as an owned parameter, a
  struct only in the interface that defines it, and returns only a struct of its own interface; an implemented
  interface takes the owned handles its WIT says. A borrowed parameter, `borrow<t>`, is not
  derived: the runtime refuses it at installation today ("borrow requires a dedicated interpreter
  value adapter"). No `stream` or `future`. `()` is nothing only as a function's result or a
  side of a `Result`, which WIT then leaves out: `Result<T, ()>` is `result<t>`, `Result<(), E>` is
  `result<_, e>` and `Result<(), ()>` is `result`; `Option<()>` has no equivalent, use `bool`.
- An implemented interface is fixed by its WIT: the `impl` adds no function to it, and a record
  of a consumed package is not a state; a state copies it into a record of the module.
- A type that enters the WIT is replaced by the type the bindings generate, which takes the
  author's `#[derive(..)]`s and the helper attributes they read: the single-name attributes on the
  type, its fields and its cases, such as `#[default]` or `#[strum(..)]`, but none of Rust's own
  attributes, such as `#[allow(..)]` or `#[repr(..)]`. A derive of what the bindings implement
  themselves is left to them: `Debug` for every type; `Clone` for a record or a variant that holds
  no resource handle, and `Copy` when it holds no string or list either; `Clone`, `Copy`,
  `PartialEq`, `Eq`, `PartialOrd` and `Ord` for an enum; `Display` and `Error` for the error of a
  function's result; and `serde`'s derives in an Orb with a state. A derive of another crate named
  after one of those traits would implement it twice, and a derive of the standard library that the
  type's contents rule out could not apply, `Clone`, `Copy`, a comparison or `Hash` on a type
  holding a resource handle, `Copy` on one holding a string or a list: both are compile errors on
  the derive.
- The bindings read a derive's path in their own module, so the macro names each derive from its
  crate, `::strum::EnumIter` for `EnumIter` after `use strum::EnumIter;`, following the `use` and
  `extern crate` declarations of the type's module, `self::`, `super::` and the Orb's sub-modules;
  a derive of the standard library is named from `core`. A single name the module does not import
  by name, other than a derive of the standard library, which only a glob import could bring, or a
  path leaving the Orb module through `super::`, is a compile error saying what to write instead.
- What the replacement would drop without a word is refused on a type that enters the WIT: an
  explicit discriminant, which a WIT enum has no room for, a `#[serde(..)]` attribute, which would
  change a state's JSON, a `#[cfg_attr(..)]`, under which a derive or its helper would come and go,
  and a helper attribute on the payload of a case, which the bindings have no place for.
- Keep no `wit/` directory in the crate: `wit-bindgen` loads it on its own and finds the package
  twice.

## Async implementations of existing WIT

An implemented interface keeps the calling convention written in its WIT. Implement an
`async func` with `pub async fn`, including resource methods and static factories. The macro
forwards the future with `.await` through the official `wit-bindgen` async bindings; it does
not run an executor or block the guest thread. Enable `wit-bindgen`'s `async` feature in the
guest crate.

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
