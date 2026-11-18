use crate::Error;
use std::collections::{BTreeMap, BTreeSet};

/// Host-local identities are never reused. Generated IDs fit exactly in a JavaScript Number;
/// zero is reserved for an absent identity.
pub type Id = u64;
/// Largest generated identity, shared by every adapter without JSON precision loss.
pub const MAX_ID: Id = (1 << 53) - 1;
pub type Version = u32;

/// Stable identity. Tuple boundaries are preserved; keys are not delimiter-joined.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct Address {
    pub service: String,
    pub key: Vec<String>,
    pub principal: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Lifetime {
    Referenced,
    Host,
}

impl Lifetime {
    /// Return the stable wire name for this variant.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Referenced => "referenced",
            Self::Host => "host",
        }
    }
}

/// The members every reference to a service object has, which the kernel refuses as method names
/// when it installs an Orb, in the lowerCamelCase spelling of the official JavaScript bindings.
pub const RESERVED_MEMBERS: [&str; 6] = ["then", "release", "state", "ready", "status", "opaque"];

/// Whether a service method's name is a member every reference has, one of [`RESERVED_MEMBERS`],
/// which the kernel refuses when it installs an Orb.
pub fn reserved_member(name: &str) -> bool {
    RESERVED_MEMBERS.contains(&name)
}

/// Whether the kernel admits the version of a state or of a resource's contract: versions start
/// at 1.
pub const fn valid_version(version: Version) -> bool {
    version != 0
}

/// Whether the kernel admits a service's objects of `lifetime` lingering `linger_ms` once
/// unreferenced: a host-lifetime object lives until the host closes, and never lingers.
pub const fn admits_linger(lifetime: Lifetime, linger_ms: u32) -> bool {
    !matches!(lifetime, Lifetime::Host) || linger_ms == 0
}

/// The principal the host creates host-lifetime objects for itself as, when it installs the Orb.
pub const HOST_PRINCIPAL: &str = "host";

/// Whether the kernel admits an object of `lifetime` acquired by `principal`: a host-lifetime
/// object belongs to the host, [`HOST_PRINCIPAL`].
pub fn admits_principal(lifetime: Lifetime, principal: &str) -> bool {
    !matches!(lifetime, Lifetime::Host) || principal == HOST_PRINCIPAL
}

/// Whether the kernel admits an object of `lifetime` at `address`: a host-lifetime object lives
/// at the key `main` of the host principal (`admits_principal`).
pub fn admits_address(lifetime: Lifetime, address: &Address) -> bool {
    !matches!(lifetime, Lifetime::Host)
        || (address.key == ["main"] && admits_principal(lifetime, &address.principal))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Service {
    /// Canonical contract identity when this service participates in typed ABI matching.
    pub signature: Option<String>,
    pub name: String,
    pub methods: BTreeSet<String>,
    pub lifetime: Lifetime,
    /// Keeps an unreferenced object and its tasks live until this idle duration expires.
    pub linger_ms: u32,
}

impl Service {
    /// Create a declaration with explicit identity and default options.
    pub fn new(
        name: impl Into<String>,
        methods: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            signature: None,
            name: name.into(),
            methods: methods.into_iter().map(Into::into).collect(),
            lifetime: Lifetime::Referenced,
            linger_ms: 0,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Requirement {
    pub signatures: BTreeMap<String, String>,
    pub hook_signatures: BTreeMap<String, String>,
    pub orb: String,
    pub api_version: Version,
    pub methods: BTreeMap<String, BTreeSet<String>>,
    pub hooks: BTreeSet<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Orb {
    pub name: String,
    pub api_version: Version,
    pub services: Vec<Service>,
    pub requires: Vec<Requirement>,
    pub hooks: BTreeSet<String>,
    pub hook_signatures: BTreeMap<String, String>,
    /// Executor-backed revisions own a private resource realm instead of sharing native values.
    pub isolated: bool,
}

impl Orb {
    /// Create a declaration with explicit identity and default options.
    pub fn new(name: impl Into<String>, api_version: Version) -> Self {
        Self {
            name: name.into(),
            api_version,
            services: Vec::new(),
            requires: Vec::new(),
            hooks: BTreeSet::new(),
            hook_signatures: BTreeMap::new(),
            isolated: false,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstalledOrb {
    pub id: Id,
    pub owner: Id,
    pub code_version: Version,
    pub declaration: Orb,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Loading,
    Live,
    Released,
    Gone,
}

impl Status {
    /// Return the stable wire name for this variant.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Loading => "loading",
            Self::Live => "live",
            Self::Released => "released",
            Self::Gone => "gone",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Acquisition {
    pub reference: Id,
    pub object: Id,
    pub orb: Id,
    pub code_version: Version,
    pub activate: bool,
}

/// An admitted physical invocation. Finishing its observer does not finish this invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Invocation {
    pub context: Id,
    pub id: Id,
    pub root: Id,
    pub object: Id,
    pub orb: Id,
    pub code_version: Version,
    pub method: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum Owner {
    Host,
    Orb(Id),
    Object(Id),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Resource {
    pub id: Id,
    pub owner: Owner,
    pub name: String,
    pub version: Version,
    /// Kernel-derived execution realm: zero for host and native revisions, otherwise the
    /// isolated orb revision identity. Values are reused only inside the same realm.
    pub realm: Id,
    pub code_version: Version,
    pub exclusive: bool,
    pub dependencies: Vec<Id>,
    pub value: Option<Id>,
    pub closing: bool,
}

/// Actions are executed by the adapter after a kernel operation completes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Action {
    Execute {
        program: Id,
        effects: Vec<crate::Effect>,
    },
    CancelEffects {
        program: Id,
        effects: Vec<crate::Effect>,
    },
    ProgramPrepared(Id),
    ProgramComplete {
        program: Id,
        error: Option<Error>,
    },
    DisposeObject(Id),
    CloseResource(Id),
    DropValue(Id),
    CancelRoot(Id),
    /// One invocation was cancelled by its caller: only that call and the calls it issued, never
    /// its root or sibling calls. Physical calls keep running until they finish.
    CancelCall(Id),
    CancelTask(Id),
    RetireOrb(Id),
    StateChanged {
        object: Id,
        revision: u64,
    },
    #[cfg(feature = "subscriptions")]
    SubscriptionFailed(Id),
    #[cfg(feature = "subscriptions")]
    DetachSubscription(Id),
    #[cfg(feature = "subscriptions")]
    Deliver {
        subscription: Id,
        payload: Id,
        operation: Id,
    },
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Diagnostics {
    pub objects: usize,
    pub references: usize,
    pub roots: usize,
    pub calls: usize,
    pub resources: usize,
    pub tasks: usize,
    pub orbs: usize,
    pub closing: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct Object {
    pub address: Address,
    pub orb: Id,
    pub phase: Status,
    pub references: usize,
    pub calls: usize,
    pub tasks: usize,
    pub pinned: bool,
    pub disposing: bool,
    pub preparing: bool,
    pub idle_at: Option<u64>,
    pub linger_ms: u32,
    pub gate: Option<BTreeSet<Id>>,
    pub min_version: Version,
    pub state: Option<crate::Snapshot>,
    pub dependencies: BTreeSet<Id>,
}

#[derive(Clone, Debug)]
pub(crate) struct Reference {
    pub object: Id,
    pub released: bool,
    pub expected_api: Version,
    pub methods: BTreeSet<String>,
    #[cfg(feature = "views")]
    pub allowed: Option<BTreeSet<String>>,
}

#[derive(Clone, Debug)]
pub(crate) struct Root {
    pub contexts: BTreeSet<Id>,
    pub version: Version,
    pub principal: String,
    pub active: bool,
    pub cancelled: bool,
    pub calls: usize,
    pub touched: BTreeSet<Id>,
    pub orbs: BTreeSet<Id>,
    pub cleanup: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct Task {
    pub owner: Owner,
    pub cancelled: bool,
    pub version: Version,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Reload {
    pub id: Id,
    pub code_version: Version,
    pub orbs: Vec<InstalledOrb>,
    pub objects: Vec<Id>,
    /// Candidate providers for affected service identities, shared with language adapters.
    pub providers: BTreeMap<String, Id>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Migration {
    pub object: Id,
    pub expected_revision: u64,
    pub value: Id,
    pub state_version: Version,
}
