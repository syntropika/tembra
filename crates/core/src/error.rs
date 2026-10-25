use std::fmt;

/// Machine-readable failure categories shared by language adapters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Code {
    Invalid,
    Unavailable,
    Released,
    Gone,
    Expired,
    Cancelled,
    Forbidden,
    Conflict,
    Unsupported,
    Closed,
    Exhausted,
}

impl Code {
    /// Parses a known wire category without accepting an unknown category.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "invalid" => Some(Self::Invalid),
            "unavailable" => Some(Self::Unavailable),
            "released" => Some(Self::Released),
            "gone" => Some(Self::Gone),
            "expired" => Some(Self::Expired),
            "cancelled" => Some(Self::Cancelled),
            "forbidden" => Some(Self::Forbidden),
            "conflict" => Some(Self::Conflict),
            "unsupported" => Some(Self::Unsupported),
            "closed" => Some(Self::Closed),
            "exhausted" => Some(Self::Exhausted),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Invalid => "invalid",
            Self::Unavailable => "unavailable",
            Self::Released => "released",
            Self::Gone => "gone",
            Self::Expired => "expired",
            Self::Cancelled => "cancelled",
            Self::Forbidden => "forbidden",
            Self::Conflict => "conflict",
            Self::Unsupported => "unsupported",
            Self::Closed => "closed",
            Self::Exhausted => "exhausted",
        }
    }
}

/// Built-in failure reasons. Facets may still supply their own reason strings at an adapter seam.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Reason {
    ActivationBusy,
    Address,
    AlreadyCommitted,
    AmbiguousScope,
    ApiVersion,
    Arguments,
    Bigint,
    ByteResult,
    Call,
    CleanupBusy,
    Clock,
    Context,
    Deadline,
    Dependency,
    DependencyCycle,
    DependencyService,
    Disposing,
    Driver,
    Duplicate,
    DuplicateContribution,
    DuplicateService,
    EffectResults,
    EffectsRunning,
    ExclusiveDependent,
    Executor,
    Facet,
    FacetInstalled,
    GateBusy,
    GateFacet,
    GateObject,
    Hook,
    HookDependency,
    Hooks,
    Host,
    HostAddress,
    HostResourceReload,
    Identities,
    Invocation,
    Javascript,
    Lifetime,
    Linger,
    LiveConsumer,
    LiveReference,
    LocalValue,
    Memory,
    Metadata,
    MetadataSize,
    Method,
    MigrationGate,
    MigrationRequired,
    MigrationValue,
    MissingHook,
    MissingMethod,
    NativeEffect,
    NativeMethod,
    NativeObject,
    NativeService,
    NotReady,
    Object,
    ObjectState,
    Operation,
    OperationOwner,
    Owner,
    Payload,
    Priority,
    Program,
    ProgramPhase,
    ProgramRunning,
    Reference,
    ReloadAborted,
    ReloadDeadline,
    ReloadPreparing,
    ReloadRunning,
    ReloadTicket,
    Requires,
    Resource,
    ResourceDependencies,
    ResourceDependency,
    ResourceHost,
    ResourceState,
    ResourceVersion,
    Result,
    RetainedVersions,
    RevisionExhausted,
    Root,
    RustType,
    Scope,
    ScopeClosed,
    ScopeFacet,
    ScopeOwner,
    ScopeResource,
    Serialization,
    Service,
    State,
    StateInitialized,
    StateOwner,
    StateRevision,
    StateVersion,
    Subscription,
    SubscriptionBusy,
    SubscriptionIdle,
    SubscriptionQueue,
    Superseded,
    Task,
    Telemetry,
    UnknownService,
    UpgradeConflict,
    Value,
}
impl Reason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ActivationBusy => "activation_busy",
            Self::Address => "address",
            Self::AlreadyCommitted => "already_committed",
            Self::AmbiguousScope => "ambiguous_scope",
            Self::ApiVersion => "api_version",
            Self::Arguments => "arguments",
            Self::Bigint => "bigint",
            Self::ByteResult => "byte_result",
            Self::Call => "call",
            Self::CleanupBusy => "cleanup_busy",
            Self::Clock => "clock",
            Self::Context => "context",
            Self::Deadline => "deadline",
            Self::Dependency => "dependency",
            Self::DependencyCycle => "dependency_cycle",
            Self::DependencyService => "dependency_service",
            Self::Disposing => "disposing",
            Self::Driver => "driver",
            Self::Duplicate => "duplicate",
            Self::DuplicateContribution => "duplicate_contribution",
            Self::DuplicateService => "duplicate_service",
            Self::EffectResults => "effect_results",
            Self::EffectsRunning => "effects_running",
            Self::ExclusiveDependent => "exclusive_dependent",
            Self::Executor => "executor",
            Self::Facet => "facet",
            Self::FacetInstalled => "facet_installed",
            Self::GateBusy => "gate_busy",
            Self::GateFacet => "gate_facet",
            Self::GateObject => "gate_object",
            Self::Hook => "hook",
            Self::HookDependency => "hook_dependency",
            Self::Hooks => "hooks",
            Self::Host => "host",
            Self::HostAddress => "host_address",
            Self::HostResourceReload => "host_resource_reload",
            Self::Identities => "identities",
            Self::Invocation => "invocation",
            Self::Javascript => "javascript",
            Self::Lifetime => "lifetime",
            Self::Linger => "linger",
            Self::LiveConsumer => "live_consumer",
            Self::LiveReference => "live_reference",
            Self::LocalValue => "local_value",
            Self::Memory => "memory",
            Self::Metadata => "metadata",
            Self::MetadataSize => "metadata_size",
            Self::Method => "method",
            Self::MigrationGate => "migration_gate",
            Self::MigrationRequired => "migration_required",
            Self::MigrationValue => "migration_value",
            Self::MissingHook => "missing_hook",
            Self::MissingMethod => "missing_method",
            Self::NativeEffect => "native_effect",
            Self::NativeMethod => "native_method",
            Self::NativeObject => "native_object",
            Self::NativeService => "native_service",
            Self::NotReady => "not_ready",
            Self::Object => "object",
            Self::ObjectState => "object_state",
            Self::Operation => "operation",
            Self::OperationOwner => "operation_owner",
            Self::Owner => "owner",
            Self::Payload => "payload",
            Self::Priority => "priority",
            Self::Program => "program",
            Self::ProgramPhase => "program_phase",
            Self::ProgramRunning => "program_running",
            Self::Reference => "reference",
            Self::ReloadAborted => "reload_aborted",
            Self::ReloadDeadline => "reload_deadline",
            Self::ReloadPreparing => "reload_preparing",
            Self::ReloadRunning => "reload_running",
            Self::ReloadTicket => "reload_ticket",
            Self::Requires => "requires",
            Self::Resource => "resource",
            Self::ResourceDependencies => "resource_dependencies",
            Self::ResourceDependency => "resource_dependency",
            Self::ResourceHost => "resource_host",
            Self::ResourceState => "resource_state",
            Self::ResourceVersion => "resource_version",
            Self::Result => "result",
            Self::RetainedVersions => "retained_versions",
            Self::RevisionExhausted => "revision_exhausted",
            Self::Root => "root",
            Self::RustType => "rust_type",
            Self::Scope => "scope",
            Self::ScopeClosed => "scope_closed",
            Self::ScopeFacet => "scope_facet",
            Self::ScopeOwner => "scope_owner",
            Self::ScopeResource => "scope_resource",
            Self::Serialization => "serialization",
            Self::Service => "service",
            Self::State => "state",
            Self::StateInitialized => "state_initialized",
            Self::StateOwner => "state_owner",
            Self::StateRevision => "state_revision",
            Self::StateVersion => "state_version",
            Self::Subscription => "subscription",
            Self::SubscriptionBusy => "subscription_busy",
            Self::SubscriptionIdle => "subscription_idle",
            Self::SubscriptionQueue => "subscription_queue",
            Self::Superseded => "superseded",
            Self::Task => "task",
            Self::Telemetry => "telemetry",
            Self::UnknownService => "unknown_service",
            Self::UpgradeConflict => "upgrade_conflict",
            Self::Value => "value",
        }
    }
}
impl From<Reason> for String {
    fn from(reason: Reason) -> Self {
        reason.as_str().into()
    }
}
impl std::fmt::Display for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An operation failure. External effects performed by an adapter are not rolled back.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Error {
    pub code: Code,
    pub reason: String,
    pub message: String,
}

impl Error {
    pub fn new(code: Code, reason: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code,
            reason: reason.into(),
            message: message.into(),
        }
    }
    pub fn invalid(reason: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(Code::Invalid, reason, message)
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}/{}: {}",
            self.code.as_str(),
            self.reason,
            self.message
        )
    }
}
impl std::error::Error for Error {}
pub type Result<T> = std::result::Result<T, Error>;
