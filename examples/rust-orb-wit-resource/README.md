# Rust Orb exporting a WIT resource

This example runs two compiled Orbs. `text` hands out builders: a `Builder` is a WIT resource, a
text under construction that stays in the text Orb while whoever started it holds a handle and
pushes one character per call, until it drops the handle and the kernel destroys the value.
`speller` imports the text Orb's interface, starts a builder, spells a word through it one letter at
a time and drops it. The host is wiring only: it installs both, drives the speller, then holds two
builders itself.

| File                           | Role                                                                                              |
| ------------------------------ | ------------------------------------------------------------------------------------------------- |
| `text/src/lib.rs`              | The provider: `#[wit_resource]` on a struct, its `#[constructor]`, two `&self` methods, a service |
| `text/contract/text.wit`       | The contract `cargo tembra build` wrote: the `resource` with its constructor and methods          |
| `speller/src/lib.rs`           | The consumer: `import = "..."` on the attribute, `texts::start()` and `builder.push(..)`          |
| `speller/contract/speller.wit` | The contract of the consumer: the text Orb's interface imported, its package nested               |
| `host/src/main.rs`             | The wiring: installs both, drives the speller, holds two `Owned` builders; it defines no Orb      |
| `run.sh`                       | Build the text Orb, then the speller against its contract, show both, run the host                |

## The resource

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
    /// How many builders were destroyed, when their last owner dropped them.
    pub fn dropped() -> u32 { /* a counter incremented by Builder's Drop */ }
}
```

A struct marked `#[wit_resource]` becomes a `resource` of the interface of the service whose
methods hand it out: the function marked `#[constructor]` is its constructor, the `pub fn` taking
`&self` are its methods, the other `pub fn` its static functions; a `pub fn new` without the
marker is one of those. The struct keeps its fields and its `Drop`; a method returning the struct
returns an owned handle. Interior mutability (`Cell`, `RefCell`) is how a method changes the text,
because WIT methods borrow `self`.

```wit
interface texts {
  /// A text under construction: the caller owns it and pushes one character per call.
  resource builder {
    constructor();
    push: func(ch: char) -> u32;
    text: func() -> string;
  }
  start: func() -> builder;
  dropped: func() -> u32;
}
```

The value lives in the text Orb's component. The handle crosses to the caller, native or compiled,
and every method call travels back to the text Orb under the caller's own admission; when the last
owner drops the handle the kernel destroys the value, which runs `Drop`, and `dropped` counts it.

## The consumer

```rust
#[tembra_orb_macro::orb(
    package = "example:speller@1.0.0",
    wit = "../text/contract",
    import = "example:text/texts@1.0.0"
)]
mod speller {
    impl Spelling {
        pub fn spell(word: String) -> String {
            let builder = texts::start();
            for (index, letter) in word.chars().enumerate() {
                if index > 0 {
                    builder.push('-');
                }
                builder.push(letter);
            }
            builder.text()
            // the handle is dropped here
        }
    }
}
```

The speller imports the text Orb's interface as [rust-orb-import](../rust-orb-import/README.md)
shows for a plain method: `import = "..."` on the attribute, the provider's contract directory in
`wit`, the installer binding the import to `text` with `Install::requires`. What is new here is the
handle: `texts::start` returns a `texts::Builder` with the methods of the resource, the compiler
checks every call on it, and dropping it at the end of `spell` destroys the builder in the text Orb.

## Why a resource and not a method

The spelling is a job in several steps that belong together: each `push` adds to what the earlier
ones built. A plain method has no memory between calls, so the speller would have to send the
whole text back and forth, or the text Orb would have to keep a map from some id to a text and
trust every caller to clean up. The resource gives the job an identity, the handle, usable only by
whoever holds it; lets many run at once, the speller's builder and the host's two, each with its
own text; and cleans up by itself when the handle drops.

## Build

Run from the repository root; the speller is built against the text Orb's contract, so the text
Orb comes first.

```sh
cargo install --path crates/cargo-tembra
cargo tembra build --manifest-path examples/rust-orb-wit-resource/text/Cargo.toml
cargo tembra build --manifest-path examples/rust-orb-wit-resource/speller/Cargo.toml
```

## Run

```sh
cargo run --manifest-path examples/rust-orb-wit-resource/host/Cargo.toml -- examples/rust-orb-wit-resource/text/artifacts/text.orb.wasm examples/rust-orb-wit-resource/speller/artifacts/speller.orb.wasm
```

```text
speller spell("ciao") -> "c-i-a-o"
text    dropped()     -> 1
host    two builders  -> "hi", "yo"
text    dropped()     -> 3
```

The speller pushes seven characters, four letters and three dashes, through one builder and reads
the text; dropping the handle at the end of `spell` destroyed the builder, as `dropped` says. The
host then starts two builders of its own: `call_portable::<_, Owned>` returns each handle, the
resource's methods are called on them by their WIT names, `[method]builder.push` and
`[method]builder.text`, the pushes interleave and the two texts stay apart, and dropping both
handles at the end of the root destroys them, so `dropped` reaches 3. `run.sh` runs the steps in
order.

## Limits

A resource is handed out, never taken: a method taking a resource, owned or borrowed, as a
parameter is not expressed by the macro. Methods borrow `self`; a `&mut self` method is a compile
error. A resource belongs to one service's interface, the one named by `#[wit_resource(service =
Name)]` or the module's only service; another service of the module may return it, and its
interface `use`s the type.
