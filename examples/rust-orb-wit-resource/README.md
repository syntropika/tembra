# Rust Orb exporting a WIT resource

This example runs two compiled Orbs. `decoder` hands out sessions: a `Session` is a WIT resource,
a value that stays in the decoder while whoever opened it holds a handle and calls its methods, as
often as it likes, until it drops the handle and the kernel destroys the value. `player` imports
the decoder's interface, opens a session, decodes frame after frame through the handle and drops
it. The host is wiring only: it installs both, drives the player, then holds a session itself.

| File                           | Role                                                                                           |
| ------------------------------ | ---------------------------------------------------------------------------------------------- |
| `decoder/src/lib.rs`           | The provider: `#[wit_resource]` on a struct with `&self` methods, a service returning it       |
| `decoder/contract/decoder.wit` | The contract `cargo tembra build` wrote: the `resource` with its constructor and methods       |
| `player/src/lib.rs`            | The consumer: `import = "..."` on the attribute, `decoding::open(..)` and `session.decode(..)` |
| `player/contract/player.wit`   | The contract of the consumer: the decoder's interface imported, the decoder's package nested   |
| `host/src/main.rs`             | The wiring: installs both, drives the player, holds an `Owned` session; it defines no Orb      |
| `run.sh`                       | Build the decoder, then the player against the decoder's contract, show both, run the host     |

## The resource

```rust
/// One decoding session: the caller owns it and decodes frame after frame with it.
#[wit_resource]
pub struct Session { key: u8, frames: Cell<u32> }

impl Session {
    #[constructor]
    pub fn new(key: u8) -> Session { /* the constructor */ }
    pub fn decode(&self, frame: Vec<u8>) -> Vec<u8> { /* a method */ }
    pub fn frames(&self) -> u32 { /* a method */ }
}

#[service]
pub struct Decoding;

impl Decoding {
    /// Open a session for `key`; the caller owns the returned handle.
    pub fn open(key: u8) -> Session { Session::new(key) }
}
```

A struct marked `#[wit_resource]` becomes a `resource` of the interface of the service whose
methods hand it out: the function marked `#[constructor]` is its constructor, the `pub fn` taking
`&self` are its methods, the other `pub fn` its static functions; a `pub fn new` without the
marker is one of those. The struct keeps its fields and its `Drop`; a method
returning the struct returns an owned handle. Interior mutability (`Cell`, `RefCell`) is how a
method changes a session, because WIT methods borrow `self`.

```wit
interface decoding {
  /// One decoding session: the caller owns it and decodes frame after frame with it.
  resource session {
    constructor(key: u8);
    decode: func(frame: list<u8>) -> list<u8>;
    frames: func() -> u32;
  }
  open: func(key: u8) -> session;
  closed: func() -> u32;
}
```

The value lives in the decoder's component. The handle crosses to the caller, native or compiled,
and every method call travels back to the decoder under the caller's own admission; when the last
owner drops the handle the kernel destroys the value, which runs `Drop`.

## The consumer

```rust
#[tembra_orb_macro::orb(
    package = "example:player@1.0.0",
    wit = "../decoder/contract",
    import = "example:decoder/decoding@1.0.0"
)]
mod player {
    impl Playback {
        pub fn play(key: u8, frames: Vec<Vec<u8>>) -> Vec<u32> {
            let session = decoding::open(key);
            // session.decode(&frame) as often as needed; dropped at the end
        }
    }
}
```

`import = "ns:name/interface@version"` adds `import` of that interface to the world and
re-exports its generated module under the interface's name: `decoding::open` returns a
`decoding::Session` handle with the methods of the resource. The package comes from the directory
`wit` names, here the contract the decoder's own build wrote. At installation the host binds the
import to the decoder Orb with `Install::requires`, as for any dependency.

## Build

Run from the repository root; the player is built against the decoder's contract, so the decoder
comes first.

```sh
cargo install --path crates/cargo-tembra
cargo tembra build --manifest-path examples/rust-orb-wit-resource/decoder/Cargo.toml
cargo tembra build --manifest-path examples/rust-orb-wit-resource/player/Cargo.toml
```

## Run

```sh
cargo run --manifest-path examples/rust-orb-wit-resource/host/Cargo.toml -- examples/rust-orb-wit-resource/decoder/artifacts/decoder.orb.wasm examples/rust-orb-wit-resource/player/artifacts/player.orb.wasm
```

```text
player play(7, 3 frames) -> [0,9,7,3]
decoder closed()          -> 1
host   decode([3, 2])     -> [2,3], frames -> 1
decoder closed()          -> 2
```

The player decodes three frames through one session and reports the first byte of each, then the
session's own count; dropping the handle at the end of `play` destroyed the session, as `closed`
says. The host then opens a session of its own: `call_portable::<_, Owned>` returns the handle, the
resource's methods are called on it by their WIT names, `[method]session.decode` and
`[method]session.frames`, and dropping the handle at the end of the root destroys the second
session. `run.sh` runs the steps in order.

## Limits

A resource is handed out, never taken: a method taking a resource, owned or borrowed, as a
parameter is not expressed by the macro. Methods borrow `self`; a `&mut self` method is a compile
error. A resource belongs to one service's interface, the one named by `#[wit_resource(service =
Name)]` or the module's only service; another service of the module may return it, and its
interface `use`s the type.
