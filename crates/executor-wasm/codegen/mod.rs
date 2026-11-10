//! Generate WASI bindings during the host build; no WIT parser enters a source Orb.
use std::collections::{BTreeMap, BTreeSet};
use wit_parser::{
    Function, FunctionKind, Handle, InterfaceId, Resolve, Type, TypeDefKind, TypeId, TypeOwner,
};

mod dispatch;
mod qualify;
mod typescript;
mod values;

/// Interfaces bound completely, every function in declaration order. Each function must pass
/// the build-time ownership qualification. `wasi:clocks/timezone` is deliberately absent: it is
/// `@unstable(feature = clocks-timezone)` and the pinned upstream has no implementation, so the
/// host refuses it as an interface that is not bound.
const COMPLETE: &[&str] = &[
    "wasi:clocks/monotonic-clock@0.3.0",
    "wasi:clocks/system-clock@0.3.0",
    "wasi:random/random@0.3.0",
    "wasi:random/insecure@0.3.0",
    "wasi:random/insecure-seed@0.3.0",
    "wasi:filesystem/preopens@0.3.0",
    "wasi:filesystem/types@0.3.0",
    "wasi:cli/environment@0.3.0",
    "wasi:cli/exit@0.3.0",
    "wasi:cli/stdin@0.3.0",
    "wasi:cli/stdout@0.3.0",
    "wasi:cli/stderr@0.3.0",
    "wasi:cli/terminal-input@0.3.0",
    "wasi:cli/terminal-output@0.3.0",
    "wasi:cli/terminal-stdin@0.3.0",
    "wasi:cli/terminal-stdout@0.3.0",
    "wasi:cli/terminal-stderr@0.3.0",
    "wasi:http/types@0.3.0",
    "wasi:http/client@0.3.0",
    "wasi:sockets/types@0.3.0",
    "wasi:sockets/ip-name-lookup@0.3.0",
];

/// Mirror of the upstream bindgen `store` entries (`wasmtime-wasi` `p3/bindings.rs`). If this
/// drifts, the generated trait call no longer type-checks and the build fails.
const STORE: &[&str] = &[
    "wasi:filesystem/types@0.3.0#[method]descriptor.read-via-stream",
    "wasi:filesystem/types@0.3.0#[method]descriptor.write-via-stream",
    "wasi:filesystem/types@0.3.0#[method]descriptor.append-via-stream",
    "wasi:filesystem/types@0.3.0#[method]descriptor.read-directory",
    "wasi:cli/stdin@0.3.0#read-via-stream",
    "wasi:cli/stdout@0.3.0#write-via-stream",
    "wasi:cli/stderr@0.3.0#write-via-stream",
    "wasi:http/client@0.3.0#send",
    "wasi:http/types@0.3.0#[static]request.new",
    "wasi:http/types@0.3.0#[static]request.consume-body",
    "wasi:http/types@0.3.0#[static]response.new",
    "wasi:http/types@0.3.0#[static]response.consume-body",
    "wasi:sockets/types@0.3.0#[method]tcp-socket.listen",
    "wasi:sockets/types@0.3.0#[method]tcp-socket.send",
    "wasi:sockets/types@0.3.0#[method]tcp-socket.receive",
];

/// WIT-synchronous functions whose upstream implementation is Rust-`async` (the bindgen `async`
/// flag): the generated call is awaited on the synchronous import, so the guest stays suspended
/// on its fiber until upstream finishes, and WIT calling conventions are unchanged.
const AWAITED: &[&str] = &[
    "wasi:sockets/types@0.3.0#[method]tcp-socket.bind",
    "wasi:sockets/types@0.3.0#[method]tcp-socket.listen",
    "wasi:sockets/types@0.3.0#[static]udp-socket.create",
    "wasi:sockets/types@0.3.0#[method]udp-socket.bind",
    "wasi:sockets/types@0.3.0#[method]udp-socket.connect",
];

/// Store functions whose output streams the source path reads through the private stream
/// adapter, so dropping the guest's stream closes the upstream producer even while it is idle.
/// Streams of owned handles are always adapted (they are only produced by `listen`).
const ADAPTED: &[&str] = &["wasi:sockets/types@0.3.0#[method]tcp-socket.receive"];

/// Resources whose upstream `drop` is a store function (`Host<R>WithStore::drop`); they close
/// through the typed close queue wherever a Store is held.
const STORE_DROPS: &[&str] = &[
    "wasi:http/types@0.3.0#request",
    "wasi:http/types@0.3.0#response",
];

/// Resources whose drop also forgets the send writer the stream adapter prepared for them, so a
/// dropped socket's writer is closed and never published under a reused rep.
const PREPARED_SENDS: &[&str] = &["wasi:sockets/types@0.3.0#tcp-socket"];

/// Upstream bindings root per WASI package; HTTP lives in `wasmtime-wasi-http`.
fn bindings(package: &str) -> &'static str {
    if package == "http" {
        "wasmtime_wasi_http::p3::bindings"
    } else {
        "p3::bindings"
    }
}

/// Byte-list results bounded by a WIT `max-len` style parameter: `(interface, function,
/// parameter, reply framing bytes)`. The parameter is capped so the encoded reply fits in one
/// standard packet instead of allocating an oversized result.
const CAPPED: &[(&str, &str, &str, usize)] = &[
    ("wasi:random/random@0.3.0", "get-random-bytes", "max-len", 4),
    (
        "wasi:random/insecure@0.3.0",
        "get-insecure-random-bytes",
        "max-len",
        4,
    ),
];

/// Error types that upstream configures as `TrappableError`.
const TRAPPABLE: &[&str] = &[
    "wasi:filesystem/types@0.3.0#error-code",
    "wasi:http/types@0.3.0#error-code",
    "wasi:http/types@0.3.0#header-error",
    "wasi:http/types@0.3.0#request-options-error",
    "wasi:sockets/types@0.3.0#error-code",
];

/// Upstream host view per WASI package: `(package, Accessor data type, fully qualified view
/// getter, blocking, budgeted)`. Blocking packages hold a shared filesystem admission permit per
/// task. Budgeted packages' resources and streams hold file descriptors, so each draws one unit
/// of the open-file budget. Getters are fully qualified so the generated code needs no trait
/// imports.
const VIEWS: &[(&str, &str, &str, bool, bool)] = &[
    (
        "random",
        "WasiRandom",
        "<State as wasmtime_wasi::random::WasiRandomView>::random",
        false,
        false,
    ),
    (
        "clocks",
        "WasiClocks",
        "<State as wasmtime_wasi::clocks::WasiClocksView>::clocks",
        false,
        false,
    ),
    (
        "filesystem",
        "WasiFilesystem",
        "<State as wasmtime_wasi::filesystem::WasiFilesystemView>::filesystem",
        true,
        true,
    ),
    (
        "cli",
        "WasiCli",
        "<State as wasmtime_wasi::cli::WasiCliView>::cli",
        false,
        false,
    ),
    (
        "http",
        "WasiHttp",
        "<State as wasmtime_wasi_http::WasiHttpView>::http",
        false,
        false,
    ),
    (
        "sockets",
        "crate::sockets::CallSockets",
        "<State as wasmtime_wasi::sockets::WasiSocketsView>::sockets",
        false,
        true,
    ),
];

/// Interfaces served by a Tembra implementation of the standard upstream trait instead of the
/// package view: `(interface, Accessor data type, view getter)`. Name lookup runs on Tembra's
/// bounded, tracked resolver (see `crate::dns`), never on untracked upstream lookup jobs.
const OVERRIDES: &[(&str, &str, &str)] = &[(
    "wasi:sockets/ip-name-lookup@0.3.0",
    "crate::dns::Dns",
    "<State as crate::dns::DnsView>::dns",
)];

/// Emit typed native dispatch and the identical guest/browser value metadata.
pub(super) fn generate(resolve: &Resolve) -> (String, String) {
    let mut generator = Generator {
        resolve,
        types: Vec::new(),
        type_index: BTreeMap::new(),
        resources: Vec::new(),
        helpers: String::new(),
        helpers_seen: BTreeSet::new(),
    };
    let (rust, methods, writable) = generator.generate();
    let typescript = generator.typescript(&methods, &writable);
    (rust, typescript)
}

struct Method<'a> {
    interface: String,
    function: &'a Function,
    resource: Option<String>,
    /// `static` or `constructor` for resource functions without `self`.
    kind: Option<&'static str>,
    asynchronous: bool,
    params: Vec<usize>,
    result: Option<usize>,
}

struct Generator<'a> {
    resolve: &'a Resolve,
    types: Vec<String>,
    type_index: BTreeMap<String, usize>,
    /// Qualified resource names; the index is the handle kind.
    resources: Vec<(String, String)>,
    /// One generated encoder/decoder function per named WIT type.
    helpers: String,
    helpers_seen: BTreeSet<String>,
}

/// Convert a WIT identifier to a Rust snake-case field or function name.
fn snake(name: &str) -> String {
    let name = name.replace('-', "_").to_ascii_lowercase();
    if matches!(name.as_str(), "type" | "self") {
        format!("{name}_")
    } else {
        name
    }
}

/// Convert a WIT identifier to a Rust type or variant name.
fn camel(name: &str) -> String {
    name.split('-')
        .map(|part| {
            let mut chars = part.chars();
            // WIT words are all lowercase or all uppercase (`DNS`); bindgen spells both `Dns`.
            chars.next().map_or(String::new(), |first| {
                first.to_ascii_uppercase().to_string() + &chars.as_str().to_ascii_lowercase()
            })
        })
        .collect()
}

/// Convert a WIT identifier to the jco JavaScript member name.
fn lower_camel(name: &str) -> String {
    let camel = camel(name);
    let mut chars = camel.chars();
    chars.next().map_or(String::new(), |first| {
        first.to_ascii_lowercase().to_string() + chars.as_str()
    })
}

impl<'a> Generator<'a> {
    /// Qualified interface name such as `wasi:filesystem/types@0.3.0`.
    fn interface_name(&self, id: InterfaceId) -> String {
        self.resolve.id_of(id).expect("named interface")
    }

    /// Rust module path of an interface inside the upstream p3 bindings.
    fn module(&self, id: InterfaceId) -> String {
        let interface = &self.resolve.interfaces[id];
        let package = &self.resolve.packages[interface.package.expect("package")];
        format!(
            "{}::{}::{}",
            bindings(&package.name.name),
            snake(&package.name.name),
            snake(interface.name.as_deref().expect("named interface"))
        )
    }

    /// Follow aliases to the defining type.
    fn peel(&self, ty: Type) -> Type {
        match ty {
            Type::Id(id) => match &self.resolve.types[id].kind {
                TypeDefKind::Type(inner) => self.peel(*inner),
                _ => ty,
            },
            other => other,
        }
    }

    /// Rust path of a named type definition.
    fn rust_path(&self, id: TypeId) -> String {
        let definition = &self.resolve.types[id];
        let TypeOwner::Interface(owner) = definition.owner else {
            panic!("standard types are owned by interfaces");
        };
        format!(
            "{}::{}",
            self.module(owner),
            camel(definition.name.as_deref().expect("named type"))
        )
    }

    /// Qualified type name for trappable and resource tables.
    fn qualified(&self, id: TypeId) -> String {
        let definition = &self.resolve.types[id];
        let TypeOwner::Interface(owner) = definition.owner else {
            panic!("standard types are owned by interfaces");
        };
        format!(
            "{}#{}",
            self.interface_name(owner),
            definition.name.as_deref().unwrap_or("")
        )
    }

    /// Handle kind of a resource; the index is stable for one build.
    fn resource(&mut self, id: TypeId) -> usize {
        let id = match self.peel(Type::Id(id)) {
            Type::Id(id) => id,
            _ => unreachable!(),
        };
        let name = self.qualified(id);
        if let Some(index) = self.resources.iter().position(|(known, _)| *known == name) {
            return index;
        }
        self.resources.push((name, self.rust_path(id)));
        self.resources.len() - 1
    }
}
