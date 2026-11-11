# tembra-orb-macro

`#[orb]` declares a compiled Orb from a Rust module. The module is the contract: the macro derives
the WIT from it and generates the bindings, so an author writes neither a `.wit` file nor a build
script. [`cargo tembra build`](../cargo-tembra/README.md) compiles the crate, reads the contract the
macro embedded in the component and packages the artifact. The component is of the kind a
TypeScript Orb is componentized into: next to its services it exports the lifecycle of the kernel's
private `tembra:orb` package, which the macro implements from the module, and it imports the
package's host operations to reach the kernel; see [the kernel's lifecycle](#the-kernels-lifecycle).
The [service example](../../examples/rust-orb-service/README.md) shows the whole path; the
[hook contribution example](../../examples/rust-orb-hook-contribution/README.md) adds a
contribution to another Orb's hook and keeps its counts in a resource of the Orb's root; the
[hook declaration example](../../examples/rust-orb-hook-declaration/README.md) declares a hook; the
[state example](../../examples/rust-orb-state/README.md) gives a service kernel-managed state, and
the [lifecycle example](../../examples/rust-orb-lifecycle/README.md) writes the same counter by
hand, without the macro.

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

| In the module                                                                       | In the WIT                                                                                                                                            |
| ----------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------- |
| the module name                                                                     | the `world`                                                                                                                                           |
| `package = "..."` on the attribute                                                  | `package ns:name@version;`                                                                                                                            |
| `wit = "dir"` on the attribute                                                      | the directory with the WIT packages the Orb consumes, relative to the crate                                                                           |
| `context` on the attribute                                                          | nothing in the WIT: a `context` module for the call's principal and cancellation, the object's address and the log                                    |
| a unit struct with `#[service]` and an `impl` block of `pub fn`                     | an exported `interface`, in kebab-case                                                                                                                |
| `#[service("ns:name/interface@version")]` on a unit struct                          | `export` of that interface of a consumed package, implemented by the `impl` block                                                                     |
| `#[service(lifetime = host)]` or `#[service(linger_ms = n)]`                        | nothing in the WIT: the ownership policy of the service's objects, reported by `lifecycle.describe`                                                   |
| `import = "ns:name/interface@version"` on the attribute                             | `import` of that interface of a consumed package, bound to a provider at installation                                                                 |
| a struct with `#[wit_resource]` and an `impl` block of `pub fn`                     | a `resource` of its service's interface: the `#[constructor]` function its constructor, the `&self` functions its methods                             |
| each `pub fn` without `self`                                                        | a `func`; name and parameters in kebab-case                                                                                                           |
| a struct with named fields, when a service method, a WIT resource or a hook uses it | a `record`                                                                                                                                            |
| an enum, when a service method, a WIT resource or a hook uses it                    | an `enum` when every variant is empty, otherwise a `variant`                                                                                          |
| `i32`, `u32`, `String`, `Vec<T>`, `Option<T>`, `Result<T, E>`, tuples               | `s32`, `u32`, `string`, `list<T>`, `option<T>`, `result<T, E>`, `tuple<...>`                                                                          |
| a free `pub fn` with `#[hook("ns:name/hook@version", priority = n)]`                | `import` and `export` of that hook interface in the world: a contribution                                                                             |
| a unit struct with `#[hook("ns:name/hook@version", composition = c)]`               | `export` of that hook interface in the world: a declaration                                                                                           |
| a unit struct with `#[hook(composition = c, input = Type)]`                         | a hook `interface` of the package under the struct's name, its callback `apply` and the types the input reaches, exported by the world: a declaration |
| a struct with named fields and `#[state(service = Name, version = n)]`              | a `record` when a method uses it, nothing else: the state each object of the service declares to the kernel in `lifecycle.create`                     |
| a struct with `#[resource(version = n)]` and a function marked `#[open]`            | nothing in the WIT: a resource the Orb's root declares in `lifecycle.create` and the kernel opens through `lifecycle.open`                            |
| `#[validate]` on a `fn(&self) -> Result<(), String>` of a `#[state]` struct         | nothing in the WIT: the state's rule, run on every update and by `lifecycle.validate` on reload                                                       |
| `#[close]` on a `fn(self)` of a `#[resource]` struct                                | nothing in the WIT: run through `lifecycle.close` at teardown                                                                                         |
| an inline `mod`                                                                     | nothing of its own: its items are read as the Orb's, grouped with what belongs to them                                                                |
| `///` comments                                                                      | `///` comments in the WIT                                                                                                                             |

Nothing is exported by accident. A service is a unit struct marked `#[service]`; the `pub fn` of
its `impl` blocks are its methods, private functions stay private. A struct or enum enters the WIT
only when a service method, a function of a `#[wit_resource]` or the input of a derived hook uses
it, directly or through the fields of another exported type; the other types of the module, and
unit structs without the marker, remain ordinary Rust. A method that names a type the module does
not declare is a compile error, and so is a service with the name of the module, because WIT
refuses a world and an interface of one name.

Nothing is bound by name either. The functions the generated code calls by role, a state's rule,
a root resource's opening and closing and a WIT resource's constructor, carry a marker,
`#[validate]`, `#[open]`, `#[close]` or `#[constructor]`, whatever their names and visibility; a
function without a marker is ordinary Rust, and a marker on a function of the wrong shape, or of
a struct of another kind, is a compile error.

## The kernel's lifecycle

Next to its services the component exports `tembra:orb/lifecycle@0.1.0` and imports
`tembra:orb/host@0.1.0`, two interfaces of the kernel's private package `tembra:orb@0.1.0`, the
package a componentized JavaScript Orb is built against too. The host links it itself: it calls the
lifecycle around the services and answers the host operations, so the installer binds and grants
nothing for them. The macro implements the lifecycle from the marked items:

- `describe` reports the Orb's name and API version, the services with their methods, lifetime and
  linger, the hooks the module declares, with their composition, and the hooks it contributes to,
  with their priority. The host checks the description against the contract before anything runs.
- `create` runs once for the Orb's root and once for each service object. The root declares its
  hooks, its resources and, for each `import = "..."`, a reference to the provider's `main` object;
  an object of a service with a state declares that state, with its validator. With `context`, each
  of them also keeps the address the kernel gave it.
- `validate` is the validator of every state. On a reload the new instance is asked about the state
  it inherits: the JSON must parse as the state type, then pass the `#[validate]` function if there
  is one, so a reload whose new code cannot read the state is refused instead of trapping on the
  first read.
- `open` and `close` run the root resources' `#[open]` and `#[close]` functions; `release` forgets
  an object, or the root, that the host retired.

The name and the API version come from the crate manifest, where `cargo tembra build` reads them to
register the artifact; the host refuses an artifact registered under another name or API version
than the one the component describes. The macro includes the manifest in its expansion, so a change
to it rebuilds the component.

```toml
[package.metadata.tembra]
name = "greeter"   # default: the package name
api-version = 1    # default: 1
```

The private package never enters the contract. The WIT the macro derives, the `WIT` constant and
the contract `cargo tembra build` writes, holds the module's interfaces and its world. The bindings
are generated from a build world, `<world>-orb`, that includes the contract's world and
`tembra:orb/source@0.1.0`, the lifecycle export and the host import, and imports each hook the
module declares, the entry through which `run` asks the kernel to run it. The TypeScript build makes
the same additions with `Contract::source_world` before componentizing JavaScript, and
`cargo tembra build` packages the component with the same `Contract::source_component_artifact`.

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
name is free; its single parameter and its result are the callback's. The priority is not WIT:
`lifecycle.describe` reports it next to the hook's identity. A contribution runs in the Orb's root,
not on a service object, so what it keeps is a [resource of the root](#resources-of-the-orbs-root).

## Declaring a hook

An Orb that owns a hook marks a unit struct with the composition, one of `series`, `waterfall` and
`bail`, and the type of the callback's input; the macro derives the hook's interface from them,
under the struct's name in the Orb's package, as it derives a service's:

```rust
#[tembra_orb_macro::orb(package = "example:gate@1.0.0")]
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
the contributions of other Orbs in plan order and returns the composed result. The contract's world
exports the hook interface, which is what the portable hook contract requires of a provider, and
the macro answers that export with a callback that traps, because the contributions answer the
hook. The bindings import the interface too, through the build world only: `run` selects the hook
the root declared, with `host.select-hook`, then calls that import, and the host runs the plan
instead of calling a provider. The bindings spell the hook's types once per side, so the macro
generates the conversions `run` applies. Contributors read the hook from the provider's contract,
`wit = "../gate/contract"` and `#[hook("example:gate/guard@1.0.0", priority = n)]`; see
[rust-orb-hook-declaration](../../examples/rust-orb-hook-declaration/README.md).

A hook that is a contract of its own, written by hand because no single Orb owns it or because its
provider is native, is declared by its identity instead, with the directory holding its package:
`#[hook("example:admission/guard@1.0.0", composition = waterfall)]` under `wit = "../wit"`. The
hook's types then belong to the consumed package, re-exported under the interface's name,
`guard::Admission`, and `run` takes and returns them; the macro checks that the callback's shape
fits the composition. Which role an interface plays is not WIT either: `lifecycle.describe` lists
the hooks the module declares, with their composition, apart from its contributions, and the root's
`create` declares each of those hooks with `host.hook-declare`. An Orb declares a hook or
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
again soon finds the same object. A host-lifetime object cannot linger. The policy is not WIT:
`lifecycle.describe` reports it with the service, as the lifetime and the linger of a TypeScript
service, and the kernel applies it.

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
        /// Add `by` to the count and return the new count, or the kernel's refusal.
        pub fn add(by: i64) -> Result<Count, String> {
            Count::update(|count| {
                count.total += by;
                count.steps += 1;
            })
        }
    }
}
```

The macro gives the state type two associated functions. `Count::read()` returns the state of the
object running the current call. `Count::update(|count| ...)` changes it and stores it, and returns
`Result<Count, String>`: the stored value, or the refusal, the state's rule's or the kernel's, in
which case the kernel keeps the previous value. The initial value is `Default::default()`, which the
macro implements field by field. Behind them is the lifecycle: when the kernel creates a service
object, `create` declares the state of that service with `host.state-declare`, the initial value
as JSON, the version and the validator, and keeps the handle the kernel returns; `read` and
`update` reach the state through `host.state-read` and `host.state-update` with that handle. The
contract does not change, and the installer requires and grants nothing for the state.

A state may carry its own rule: a function of its `impl` block marked `#[validate]`, of the shape
`fn(&self) -> Result<(), String>`, under any name. `Count::update` runs it on the closure's result
before the kernel stores it, and returns its refusal as the `Err`, the previous value left in
place. `lifecycle.validate` runs it too on a reload, when the new instance is asked whether it
accepts the value it inherits: a refusal fails the reload and the running revision keeps serving,
as the [state example](../../examples/rust-orb-state/README.md) shows for an update. Without the
marker, a reload only checks that the retained JSON still reads as the state, and a function named
`validate` is ordinary Rust.

Two rules follow from the kernel's model. A service's state is reached only by the methods of that
service: a call runs on one object, and the accessors, which find the state through the object
running the call, trap when that object has no state or belongs to another service. An Orb's root
is not an object, so a hook contribution, which runs in the root, has no state to read or write;
what a contribution needs or counts is a resource of the root, below. One state per service
object: group what an object owns in one struct, and let observers select the fields they care
about.

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
returns its result; before the resource is opened, or after it is closed, it traps. Behind it is
the lifecycle: the root's `create` declares each resource with `host.resource-declare`, its name in
kebab-case and its version; the kernel then calls `lifecycle.open` during activation, which runs
the `#[open]` function and keeps the value in a slot of the component, and `lifecycle.close` at
teardown, which empties the slot and hands the value to the `#[close]` function. `with` asks the
kernel first, through `host.resource-read`, whether the resource is open. The value lives in the
component, one per instance, as the TypeScript guest's root resources do: a reload gives the Orb a
fresh instance whose resources the kernel opens again. A resource struct enters the WIT only when
a service method uses it; a resource may hold types the WIT has no spelling for.

## The call's context

A method runs inside a call the kernel admitted, on an object the kernel created for a key and a
principal. With `context` on the attribute the macro gives the module a `context` module of
functions the kernel answers through `tembra:orb/host`:

```rust
#[tembra_orb_macro::orb(package = "example:audit@1.0.0", context)]
mod audit {
    use alloc::{format, string::String};

    #[service]
    pub struct Auditor;

    impl Auditor {
        /// Who is calling, and which object answers.
        pub fn whoami() -> Result<String, String> {
            let object = context::object()?;
            context::log(context::Level::Info, "whoami was asked");
            Ok(format!("{} calls {:?} of {}", context::principal()?, object.key, object.principal))
        }
    }
}
```

`context::principal()` is the principal of the current call and `context::object()` the address
of the object running it, as the kernel gave it when it created the object: the key its reference
was acquired with and the principal that acquired it, or an empty key and the principal `host` in a
hook contribution, which runs in the Orb's root. `context::cancelled()` tells whether the call was
cancelled and `context::cancel()` cancels it. Each of these returns a `Result`: after a
cancellation the kernel refuses `principal` and `cancel`, and the caller decides what a refusal
means. `context::log(level, message)` writes a record the host's log observers receive, attributed
to this Orb by the host, and returns nothing. The contract does not change, and the installer
requires and grants nothing. Outside a call, in a resource's `#[open]` function for instance, the
principal and the cancellation are not available.

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
interface to the world and gives the module a module of the interface's name:

```rust
#[tembra_orb_macro::orb(
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
The module `greeting` re-exports the types and resources of the generated bindings and wraps each
freestanding function of the interface, with the signature the bindings give it: the call first
selects the reference the Orb's root declared to the provider's `main` object, with `host.select`,
because the kernel refuses a dependency call without a selection. A resource an imported function
returns is an owned handle with the resource's methods, which target the handle and need no
selection; the handle is destroyed in the provider when it drops. `greeting::hello` is an ordinary
function here and a call through the kernel at run time, under the caller's principal. Which Orb
answers is the installer's choice: it binds the import to a provider Orb with `Install::requires`,
as for any dependency. The macro records the directory in a `tembra:consumed` custom section, and
`cargo tembra build` nests the imported package whole in the contract, so the kernel checks the
provider against the same signature. See [rust-orb-import](../../examples/rust-orb-import/README.md)
and, for a handed-out resource,
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

## Organizing the module in sub-modules

The Orb module may hold sub-modules, written inline; the macro reads them as part of the Orb and
leaves them in place, so a service can live with what belongs to it:

```rust
#[tembra_orb_macro::orb(package = "example:ledger@1.0.0")]
mod ledger {
    pub mod pinned {
        /// The balance of one pinned ledger object, kept by the kernel.
        #[state]
        pub struct Balance { pub total: i64 }

        #[service(lifetime = host)]
        pub struct Pinned;

        impl Pinned {
            pub fn add(amount: i64) -> i64 {
                let balance = Balance::update(|balance| balance.total += amount);
                balance.expect("the kernel keeps the balance").total
            }
        }
    }

    pub mod passing { /* its own Balance, its own service */ }
}
```

A state or a WIT resource that names no service belongs to the one service of its sub-module, else
to the one service of the Orb. A service's methods are the `pub fn` of the `impl` blocks of its own
module, and a marked function goes in the module of its struct. The bridges the macro generates for
an item go in the item's module, so the author's types resolve where they were written; what the
Orb module itself reaches, a state and its fields, a root resource, a WIT resource and a marked
function, is `pub` or `pub(super)` in a sub-module. A name that enters the WIT is one across the
modules, because the package has one namespace: the services, the hooks, the resources and the
types a method or a hook mentions; a state that stays out of the WIT may repeat its name module by
module, as `Balance` does, and a module's own name wins when a method mentions it. The modules the
macro generates in the Orb module, `context`, an imported interface, a hook declared from a
consumed package, are reached from a sub-module through `super::`. See
[rust-orb-service-lifetime](../../examples/rust-orb-service-lifetime/README.md).

## What it generates

The macro rewrites the module with:

1. `pub const WIT: &str`, the derived WIT text;
2. the same text as a `tembra:wit` custom section of the Wasm module and, with `wit = "dir"`, the
   directory as written on the attribute as a `tembra:consumed` section. `cargo tembra build` reads
   them and packages the contract exactly as written here, with the `///` comments the binary type
   section loses, the imports of the contributed hooks, which the linker drops because nothing
   calls them, and every consumed package nested whole, not the part the component happens to
   call;
3. the `no_std` prelude for the Wasm target, `#[global_allocator]` on `dlmalloc` and a
   `#[panic_handler]` that writes the panic's message to the host's log, at level error, then
   traps, under `#[cfg(target_arch = "wasm32")]`; on the host the crate builds with std, so
   `cargo check` and rust-analyzer work without a Wasm target;
4. `wit_bindgen::generate!({ inline, world: "<world>-orb", generate_all })`, the official macro
   with the WIT as a string: the contract, the build world that adds `tembra:orb/source@0.1.0` and
   the imports of the declared hooks, and the private `tembra:orb` package nested after them; plus
   `path` when the Orb consumes other packages and, with a state, `serde` derives on the generated
   types;
5. `pub use exports::...::{Stats}`: the author's record and enum definitions are replaced by the
   generated types with the same names and public fields, so the author's functions and the
   generated traits share one type; `pub use exports::...::guard;` for each contributed hook and
   each implemented interface, and `pub use ...::guard;`, the import module, for each hook declared
   from a consumed package;
6. a module for each imported interface, re-exporting its types and resources and wrapping each of
   its freestanding functions to select the root's reference to the provider first, and the
   `context` module with the `context` option;
7. the author's items, unchanged;
8. a private struct implementing the `Guest` trait of every interface, derived or implemented, by
   forwarding to the author's functions, hook callbacks included, with a trapping callback for each
   declared hook, and `export!`; the resource trait of each `#[wit_resource]` struct on the struct
   itself, its constructor and methods forwarded and the handles wrapped at the boundary; with a
   declared hook, `run` on its struct, which selects the hook and calls its import, and, for a hook
   derived from `input = Type`, `From` in both directions between the two sides of the bindings for
   the types it reaches;
9. the same struct implementing `tembra:orb/lifecycle`, `describe`, `create`, `release`,
   `validate`, `open` and `close`, over a hidden runtime module that keeps the handles the root
   declared, its hooks, references and resources, and the state handle of each object, by the
   environment the host gave it;
10. with a state, `Default` and the `read` and `update` functions on each state type, which map the
    kernel's JSON to the type with `serde_json`;
11. with a resource, one slot in the component's memory per resource, filled by the `#[open]`
    function and emptied by the `#[close]` one, and the `with` function on each resource type.

The crate must list `wit-bindgen` (features `macros`, `realloc` and `async`, because the
lifecycle's `open` and `close` are async functions) and `dlmalloc` (feature `global`) among its
dependencies, and `serde` (features `derive`, `alloc`) and `serde_json` (feature `alloc`) when it
has a state: the generated code names them. `cargo tembra new` writes such a manifest.

## Limits

- WASI interfaces are not expressed. An Orb consumes another Orb's WIT by importing an
  interface, implementing one or contributing to a hook; selecting another object by key, as a
  TypeScript Orb does with `context.get`, is not expressed, and neither is restricting a service's
  methods by principal, as an SDK service does with `access(principal)`.
- Of an imported interface, the macro wraps the freestanding functions; one taking a tuple or a
  result that holds a string or a list is refused at compile time, and so is any consumed
  interface with async functions, imported or implemented. The constructor and the static
  functions of an imported resource are not wrapped: called directly, they reach the host without
  a selection, which it refuses. Take the resource from a function of the interface, as the
  speller of rust-orb-wit-resource takes its builder from `texts::start()`.
- A hook declared from a consumed package keeps that package's types: a method that reports its
  record returns a record of the module, converted from it. A hook derived from `input = Type`
  shares the module's types, defined in the hook's interface; its input is a record, an enum, a
  variant, a scalar, a string or a list.
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
