//! The authoritative kernel command table.
//!
//! Each entry names one operation, its typed input fields, its typed result and the core call that
//! implements it. Rust dispatch, the typed [`Command`] API, operation names and the generated
//! TypeScript call map are all derived from this table; nothing else lists the operations.

use crate::Kernel;
use crate::decode::{Decode, Input};
use crate::wire::*;
use tembra_core::{self as core, Code, Error, Id, Reason, Result};

mod context;

/// One typed kernel operation. Rust callers execute it without serializing.
pub trait Command: Sized {
    /// The stable operation name.
    const OP: KernelOp;
    /// The result type determined by this operation.
    type Output: serde::Serialize + Wire;

    /// Execute this command against the kernel's authoritative core.
    fn run(self, kernel: &Kernel) -> Result<Self::Output>;
}

/// Shape of one command for the TypeScript generator.
pub(crate) struct Shape {
    pub variant: &'static str,
    pub wire: &'static str,
    pub docs: &'static [&'static str],
    pub fields: Vec<FieldShape>,
    pub output: String,
}

macro_rules! commands {
    ($(
        $(#[doc = $doc:literal])*
        $variant:ident = $wire:literal $([$cfg:meta])? { $($field:ident: $ty:ty),* $(,)? }
            -> $output:ty => $handler:expr;
    )+) => {
        names! {
            /// Closed kernel operation names; unknown input is rejected before decoding.
            KernelOp { $($variant => $wire),+ }
        }

        impl KernelOp {
            /// Report whether this build implements the operation.
            pub const fn enabled(self) -> bool {
                match self { $(Self::$variant => commands!(@enabled $($cfg)?),)+ }
            }
        }

        /// Typed command inputs. Unknown fields, wrong types and unsafe identities are rejected.
        pub mod input {
            use super::*;
            $(
                $(#[doc = $doc])*
                #[derive(Clone, Debug, PartialEq)]
                pub struct $variant { $(pub $field: $ty,)* }
                decode_record!($variant { $($field),* });
            )+
        }

        $(
            impl Command for input::$variant {
                const OP: KernelOp = KernelOp::$variant;
                type Output = $output;
                commands!(@run [$($cfg)?] $output, $handler);
            }
        )+

        /// The typed result of any command. The operation name selects the variant.
        #[derive(Debug, serde::Serialize)]
        #[serde(untagged)]
        pub enum Reply { $($variant($output),)+ }

        impl Kernel {
            /// Decode the complete input of an enabled operation, then execute it.
            pub(crate) fn decode_run(&self, op: KernelOp, input: Input) -> Result<Reply> {
                match op {
                    $(KernelOp::$variant => input::$variant::decode(input)?.run(self).map(Reply::$variant),)+
                }
            }
        }

        /// Describe every command for the TypeScript generator.
        pub(crate) fn shapes() -> Vec<Shape> {
            vec![$(Shape {
                variant: stringify!($variant),
                wire: $wire,
                docs: &[$($doc),*],
                fields: vec![$((stringify!($field), <$ty as Wire>::OPTIONAL, <$ty as Wire>::input())),*],
                output: <$output as Wire>::ts(),
            }),+]
        }
    };
    (@enabled) => { true };
    (@enabled $cfg:meta) => { cfg!($cfg) };
    (@run [] $output:ty, $handler:expr) => {
        fn run(self, kernel: &Kernel) -> Result<$output> {
            let handler: fn(&Kernel, Self) -> Result<$output> = $handler;
            handler(kernel, self)
        }
    };
    (@run [$cfg:meta] $output:ty, $handler:expr) => {
        #[cfg($cfg)]
        fn run(self, kernel: &Kernel) -> Result<$output> {
            let handler: fn(&Kernel, Self) -> Result<$output> = $handler;
            handler(kernel, self)
        }
        #[cfg(not($cfg))]
        fn run(self, _: &Kernel) -> Result<$output> {
            Err(unavailable(Self::OP.as_str()))
        }
    };
}

commands! {
    /// Derive a child context frame carrying a typed scalar or an owner-issued reference.
    ContextDerive = "context-derive" {
        root: Handle,
        context: Option<Handle>,
        call: Option<Handle>,
        key: ContextKeyInput,
        value: Option<ContextScalar>,
        source: Option<Handle>,
    } -> Ident => |k, i| context::derive(k, i).map(Ident);
    /// Read the nearest binding of a typed context key.
    ContextValue = "context-value" {
        root: Handle,
        context: Option<Handle>,
        call: Option<Handle>,
        key: ContextKeyInput,
    } -> Option<ContextEntry> => context::value;
    /// Read context frame counters.
    ContextStats = "context-stats" {} -> ContextStats => |k, _| {
        let (live, peak, keys) = k.host.context_stats();
        Ok(ContextStats { live, peak, keys })
    };
    /// Allocate an opaque host identity.
    Allocate = "allocate" {} -> Ident => |k, _| k.host.allocate_handle().map(Ident);
    /// Describe a declared scope to the lifecycle engine.
    ScopeRegister = "scope-register" {
        scope: Handle,
        owner: OwnerInput,
        orb: Handle,
        parents: Option<Vec<Handle>>,
        dependencies: Option<Vec<Handle>>,
        resources: Option<Vec<Handle>>,
        state_version: Option<u32>,
        migrate: Option<bool>,
    } -> () => |k, i| {
        k.host.register_scope(core::Scope {
            id: i.scope.0,
            owner: i.owner.resolve()?,
            orb: i.orb.0,
            parents: ids(i.parents),
            dependencies: ids(i.dependencies),
            resources: ids(i.resources),
            state_version: i.state_version,
            migrate: i.migrate.unwrap_or(false),
            active: false,
            closing: false,
        })
    };
    /// Report whether a scope is registered.
    ScopeExists = "scope-exists" { scope: Handle } -> bool => |k, i| Ok(k.host.scope_exists(i.scope.0));
    /// List scopes owned by the host, an orb or an object.
    ScopesForOwner = "scopes-for-owner" { owner: OwnerInput } -> Vec<Ident> => |k, i| {
        Ok(idents(k.host.scopes_for_owner(i.owner.resolve()?)))
    };
    /// List scopes of one orb version.
    ScopesForOrb = "scopes-for-orb" { orb: Handle } -> Vec<Ident> => |k, i| Ok(idents(k.host.scopes_for_orb(i.orb.0)));
    /// Attach a resource to a registered scope.
    ScopeAddResource = "scope-add-resource" { scope: Handle, resource: Handle } -> () => |k, i| {
        k.host.add_scope_resource(i.scope.0, i.resource.0)
    };
    /// Start the activation program of a registered scope.
    ScopeActivate = "scope-activate" { scope: Handle } -> Ident => |k, i| k.host.activate_scope(i.scope.0).map(Ident);
    /// Start a cleanup program for scopes.
    ScopesCleanup = "scopes-cleanup" { scopes: Vec<Handle>, object: Option<Handle> } -> Ident => |k, i| {
        k.host
            .cleanup_scopes(ids(Some(i.scopes)), i.object.map(|object| object.0))
            .map(Ident)
    };
    /// Reload every registered candidate scope of a prepared ticket.
    ScopesReloadRegistered = "scopes-reload-registered" {
        ticket: Handle,
        deadline: Wide,
        now: Option<Wide>,
    } -> Ident => |k, i| {
        // Refresh host-relative time only after the complete request decoded.
        if let Some(now) = i.now {
            k.host.tick(now.0)?;
        }
        k.host
            .reload_registered_scopes(i.ticket.0, i.deadline.0)
            .map(Ident)
    };
    /// Reload explicit previous/next scope pairs of a prepared ticket.
    ScopesReload = "scopes-reload" {
        ticket: Handle,
        deadline: Wide,
        now: Option<Wide>,
        replacements: Vec<ReplacementInput>,
    } -> Ident => |k, i| {
        let pairs = i
            .replacements
            .into_iter()
            .map(|pair| core::Replacement {
                previous: pair.previous.map(|id| id.0),
                next: pair.next.map(|id| id.0),
            })
            .collect();
        if let Some(now) = i.now {
            k.host.tick(now.0)?;
        }
        k.host
            .reload_scopes(i.ticket.0, pairs, i.deadline.0)
            .map(Ident)
    };
    /// Start a prepared lifecycle program.
    ProgramRun = "program-run" { program: Handle } -> () => |k, i| k.host.run_program(i.program.0);
    /// Read a program's next step without executing callbacks.
    ProgramNext = "program-next" { program: Handle } -> Step => |k, i| {
        k.host.next_step(i.program.0).map(Step::from)
    };
    /// Acknowledge the physical settlement of every effect in the current step.
    ProgramCompleteStep = "program-complete-step" {
        program: Handle,
        results: Vec<CompletionInput>,
    } -> () => |k, i| {
        k.host.complete_step(i.program.0, i.results.into_iter().map(Into::into).collect())
    };
    /// Commit a prepared reload program.
    ProgramCommit = "program-commit" { program: Handle } -> () => |k, i| k.host.commit_program(i.program.0);
    /// Abort a reload program.
    ProgramAbort = "program-abort" { program: Handle } -> () => |k, i| k.host.abort_program(i.program.0);
    /// Forget a settled program.
    ProgramForget = "program-forget" { program: Handle } -> () => |k, i| k.host.forget_program(i.program.0);
    /// Read the capabilities compiled into this kernel.
    Features = "features" {} -> Features => |_, _| Ok(Features {
        hooks: cfg!(feature = "hooks"),
        subscriptions: cfg!(feature = "subscriptions"),
        views: cfg!(feature = "views"),
        telemetry: cfg!(feature = "telemetry"),
    });
    /// Prepare orb contracts as a reload candidate.
    Prepare = "prepare" { orbs: Vec<OrbDeclaration>, remove: Option<Vec<String>> } -> Reload => |k, i| {
        let orbs = i.orbs.into_iter().map(OrbDeclaration::into_core).collect();
        k.host.prepare(orbs, i.remove.unwrap_or_default()).map(Reload::from)
    };
    /// Gate candidate objects of a prepared ticket.
    Gate = "gate" { ticket: Handle, objects: Vec<Handle> } -> bool => |k, i| {
        k.host.gate(i.ticket.0, &ids(Some(i.objects)))
    };
    /// Gate candidate orb owners of a prepared ticket.
    GateOrbs = "gate-orbs" { ticket: Handle, owners: Vec<Handle> } -> bool => |k, i| {
        k.host.gate_orbs(i.ticket.0, &ids(Some(i.owners)))
    };
    /// Commit a prepared ticket and its state migrations.
    Commit = "commit" { ticket: Handle, migrations: Option<Vec<MigrationInput>> } -> u32 => |k, i| {
        let migrations = i
            .migrations
            .into_iter()
            .flatten()
            .map(|migration| core::Migration {
                object: migration.object.0,
                expected_revision: migration.revision.0,
                value: migration.value.0,
                state_version: migration.version,
            })
            .collect();
        k.host.commit(i.ticket.0, migrations)
    };
    /// Abort a prepared ticket.
    Abort = "abort" { ticket: Handle } -> () => |k, i| k.host.abort(i.ticket.0);
    /// Describe installed orbs.
    Orbs = "orbs" {} -> Vec<KernelOrb> => |k, _| {
        Ok(k.host.installed_orbs().iter().map(KernelOrb::from).collect())
    };
    /// Describe one installed orb version.
    Orb = "orb" { id: Handle } -> KernelOrb => |k, i| Ok(KernelOrb::from(&k.host.orb(i.id.0)?));
    /// Find an installed orb, or a candidate of a prepared ticket, by name.
    OrbNamed = "orb-named" { name: String, ticket: Option<Handle> } -> KernelOrb => |k, i| {
        let orb = match i.ticket {
            Some(ticket) => k.host.prepared_orb(ticket.0, &i.name)?,
            None => k.host.orb_named(&i.name)?,
        };
        Ok(KernelOrb::from(&orb))
    };
    /// Read the earliest kernel deadline.
    NextDeadline = "next-deadline" {} -> Option<Clock> => |k, _| Ok(k.host.next_deadline().map(Clock));
    /// Read a managed object's address.
    Address = "address" { object: Handle } -> AddressInfo => |k, i| {
        let address = k.host.object_address(i.object.0)?;
        Ok(AddressInfo {
            service: address.service,
            key: address.key,
            principal: address.principal,
        })
    };
    /// Claim a reference to a managed object.
    Acquire = "acquire" {
        service: String,
        key: Vec<String>,
        principal: String,
        api_version: Option<u32>,
        methods: Option<Vec<String>>,
    } -> Acquisition => |k, i| {
        let address = core::Address {
            service: i.service,
            key: i.key,
            principal: i.principal,
        };
        let claim = k.host.acquire(
            address,
            i.api_version.unwrap_or(0),
            i.methods.into_iter().flatten().collect(),
        )?;
        Ok(Acquisition {
            reference: Ident(claim.reference),
            object: Ident(claim.object),
            orb: Ident(claim.orb),
            code_version: claim.code_version,
            activate: claim.activate,
        })
    };
    /// Mark an activated object live.
    Ready = "ready" { object: Handle } -> () => |k, i| k.host.ready(i.object.0);
    /// Record that an object depends on a reference.
    Dependency = "dependency" { object: Handle, reference: Handle } -> () => |k, i| {
        k.host.dependency(i.object.0, i.reference.0)
    };
    /// Detach an object's dependency.
    Undepend = "undepend" { object: Handle, reference: Handle } -> () => |k, i| {
        k.host.detach_dependency(i.object.0, i.reference.0)
    };
    /// Mark an object's activation as failed.
    Fail = "fail" { object: Handle } -> () => |k, i| k.host.fail(i.object.0);
    /// Read a reference's status.
    Status = "status" { reference: Handle } -> StatusName => |k, i| {
        k.host.status(i.reference.0).map(StatusName)
    };
    /// Duplicate a claim with exactly its existing authority and API expectation.
    Duplicate = "duplicate" { reference: Handle } -> Ident => |k, i| k.host.duplicate(i.reference.0).map(Ident);
    /// Release a reference claim.
    Release = "release" { reference: Handle } -> () => |k, i| k.host.release(i.reference.0);
    /// Forget a released reference.
    Forget = "forget" { reference: Handle } -> () => |k, i| k.host.forget_reference(i.reference.0);
    /// Begin a root for a principal.
    Root = "root" { principal: String } -> Ident => |k, i| k.host.begin_root(i.principal).map(Ident);
    /// Begin a cleanup root for a principal.
    CleanupRoot = "cleanup-root" { principal: String } -> Ident => |k, i| k.host.cleanup_root(i.principal).map(Ident);
    /// Read a root's principal.
    RootPrincipal = "root-principal" { root: Handle } -> String => |k, i| k.host.root_principal(i.root.0);
    /// Enter the invocation's isolated heap under its retained private execution ancestry.
    EnterExecution = "enter-execution" { call: Handle, context: Option<Handle> } -> Ident => |k, i| {
        k.host.enter_execution(i.call.0, frame(i.context)).map(Ident)
    };
    /// Recheck a root and, when supplied, that an invocation belongs to it.
    CheckContext = "check-context" { root: Handle, call: Option<Handle> } -> () => |k, i| {
        context::check(k, i.root, i.call)
    };
    /// Reject a cancelled or ended root.
    CheckRoot = "check-root" { root: Handle } -> () => |k, i| k.host.check_root(i.root.0);
    /// End a root.
    EndRoot = "end-root" { root: Handle } -> () => |k, i| k.host.end_root(i.root.0);
    /// End logical root admission and return all resulting cleanup metadata in one exchange.
    EndRootBatch = "end-root-batch" { root: Handle } -> CallSettlement => |k, i| {
        k.host.end_root(i.root.0)?;
        Ok(CallSettlement { actions: drain(k), deadline: k.host.next_deadline().map(Clock) })
    };
    /// Cancel a root.
    CancelRoot = "cancel-root" { root: Handle } -> () => |k, i| k.host.cancel_root(i.root.0);
    /// Admit a service call through a reference.
    BeginCall = "begin-call" {
        reference: Handle,
        root: Handle,
        method: String,
        context: Option<Handle>,
    } -> Invocation => |k, i| {
        k.host
            .begin_call_with_context(i.reference.0, i.root.0, &i.method, frame(i.context), None)
            .map(Invocation::from)
    };
    /// Recheck the caller's context, then admit a service call, in one bounded exchange.
    AdmitCall = "admit-call" {
        reference: Handle,
        root: Handle,
        parent: Option<Handle>,
        method: String,
        context: Option<Handle>,
    } -> Invocation => |k, i| {
        context::check(k, i.root, i.parent)?;
        // The issuing invocation's cancellation reaches this call, and refuses it once cancelled.
        k.host
            .begin_call_with_context(
                i.reference.0,
                i.root.0,
                &i.method,
                frame(i.context),
                i.parent.map(|parent| parent.0),
            )
            .map(Invocation::from)
    };
    /// Admit an owner-bound callback operation.
    BeginOperation = "begin-operation" {
        orb: Handle,
        root: Handle,
        operation: Handle,
        object: Option<Handle>,
        context: Option<Handle>,
    } -> Invocation => |k, i| {
        k.host
            .begin_operation_with_context(
                i.orb.0,
                i.root.0,
                i.operation.0,
                i.object.map(|object| object.0),
                frame(i.context),
                None,
            )
            .map(Invocation::from)
    };
    /// Admit the single owner-bound observation of a terminal hook.
    BeginHookObservation = "begin-hook-observation" [feature = "hooks"] {
        source: Handle,
        orb: Handle,
        operation: Handle,
        object: Option<Handle>,
        hook: String,
    } -> HookObservation => |k, i| {
        let observation = k.host.begin_hook_observation(
            i.source.0,
            i.orb.0,
            i.operation.0,
            i.object.map(|object| object.0),
            &i.hook,
        )?;
        Ok(HookObservation {
            invocation: observation.invocation.into(),
            plan: idents(observation.plan.iter().copied()),
        })
    };
    /// Finish a physical call.
    FinishCall = "finish-call" { call: Handle } -> () => |k, i| k.host.finish_call(i.call.0);
    /// Tick, finish a physical call, drain its actions and read the next deadline in one exchange.
    FinishCallBatch = "finish-call-batch" { call: Handle, now: Wide } -> CallSettlement => |k, i| {
        k.host.tick(i.now.0)?;
        k.host.finish_call(i.call.0)?;
        Ok(CallSettlement {
            actions: drain(k),
            deadline: k.host.next_deadline().map(Clock),
        })
    };
    /// Describe an admitted call.
    Invocation = "invocation" { call: Handle } -> Invocation => |k, i| {
        k.host.invocation(i.call.0).map(Invocation::from)
    };
    /// Initialize an object's state.
    StateInit = "state-init" { object: Handle, value: Handle, version: u32 } -> Snapshot => |k, i| {
        k.host
            .initialize_state(i.object.0, i.value.0, i.version)
            .map(Snapshot::from)
    };
    /// Read an object's state metadata.
    Snapshot = "snapshot" { object: Handle } -> Snapshot => |k, i| {
        k.host.snapshot(i.object.0).map(Snapshot::from)
    };
    /// Adopt a new state value when the expected revision still matches.
    StateAdopt = "state-adopt" {
        call: Handle,
        object: Handle,
        revision: Wide,
        value: Handle,
    } -> Snapshot => |k, i| {
        k.host
            .adopt_state(i.call.0, i.object.0, i.revision.0, i.value.0)
            .map(Snapshot::from)
    };
    /// Begin a background task.
    Task = "task" { owner: OwnerInput } -> Ident => |k, i| k.host.begin_task(i.owner.resolve()?).map(Ident);
    /// Begin a root for a task.
    TaskRoot = "task-root" { task: Handle, principal: String } -> Ident => |k, i| {
        k.host.task_root(i.task.0, i.principal).map(Ident)
    };
    /// Request that a task stop.
    StopTask = "stop-task" { task: Handle } -> () => |k, i| k.host.stop_task(i.task.0);
    /// Finish a task.
    FinishTask = "finish-task" { task: Handle } -> () => |k, i| k.host.finish_task(i.task.0);
    /// Claim a named resource.
    Resource = "resource" {
        owner: OwnerInput,
        name: String,
        version: u32,
        exclusive: Option<bool>,
        dependencies: Option<Vec<Handle>>,
    } -> ResourceClaim => |k, i| {
        let resource = k.host.resource(
            i.owner.resolve()?,
            i.name,
            i.version,
            i.exclusive.unwrap_or(false),
            ids(i.dependencies),
        )?;
        Ok(ResourceClaim {
            id: Ident(resource.id),
            value: resource.value.map(Ident),
        })
    };
    /// Record a resource's opened value.
    ResourceOpened = "resource-opened" { resource: Handle, value: Handle } -> () => |k, i| {
        k.host.resource_opened(i.resource.0, i.value.0)
    };
    /// Read a resource's opened value.
    ResourceValue = "resource-value" { resource: Handle } -> Ident => |k, i| k.host.resource_value(i.resource.0).map(Ident);
    /// Request a resource close before an optional version.
    CloseResource = "close-resource" { resource: Handle, before: Option<u32> } -> bool => |k, i| {
        k.host.close_resource_before(i.resource.0, i.before)
    };
    /// Record that a resource closed.
    ResourceClosed = "resource-closed" { resource: Handle } -> () => |k, i| k.host.resource_closed(i.resource.0);
    /// Record that an object was disposed.
    Disposed = "disposed" { object: Handle } -> () => |k, i| k.host.disposed(i.object.0);
    /// Advance host-relative monotonic time.
    Tick = "tick" { now: Wide } -> () => |k, i| k.host.tick(i.now.0);
    /// Read the next wake time.
    NextWake = "next-wake" {} -> Option<Clock> => |k, _| Ok(k.host.next_wake().map(Clock));
    /// Read the next idle time.
    NextIdle = "next-idle" {} -> Option<Clock> => |k, _| Ok(k.host.next_idle().map(Clock));
    /// Begin host shutdown.
    Close = "close" {} -> () => |k, _| {
        k.host.close();
        Ok(())
    };
    /// Release kernel storage after shutdown.
    FinishClose = "finish-close" {} -> () => |k, _| k.host.finish_close();
    /// Read ownership counters.
    Diagnostics = "diagnostics" {} -> Diagnostics => |k, _| {
        let d = k.host.diagnostics();
        Ok(Diagnostics {
            objects: d.objects,
            references: d.references,
            roots: d.roots,
            calls: d.calls,
            resources: d.resources,
            tasks: d.tasks,
            orbs: d.orbs,
            closing: d.closing,
            code_version: k.host.version(),
        })
    };
    /// Drain pending adapter actions in order.
    Drain = "drain" {} -> Vec<Action> => |k, _| Ok(drain(k));
    /// Restrict the methods a reference may call.
    Restrict = "restrict" [feature = "views"] { reference: Handle, methods: Vec<String> } -> () => |k, i| {
        k.host.restrict(i.reference.0, i.methods.into_iter().collect())
    };
    /// Allocate a stable contribution ordinal.
    ContributionOrdinal = "contribution-ordinal" [feature = "hooks"] {} -> u32 => |k, _| {
        k.host.contribution_ordinal()
    };
    /// Register a hook contribution.
    Contribute = "contribute" [feature = "hooks"] {
        orb: Handle,
        target: String,
        hook: String,
        operation: Handle,
        priority: Option<i32>,
        ordinal: u32,
    } -> () => |k, i| {
        k.host.contribute(core::Contribution {
            orb: i.orb.0,
            target: i.target,
            hook: i.hook,
            operation: i.operation.0,
            priority: i.priority.unwrap_or(0),
            ordinal: i.ordinal,
        })
    };
    /// Read the admitted contribution plan of a hook.
    HookPlan = "hook-plan" [feature = "hooks"] { root: Handle, target: String, hook: String } -> Vec<Ident> => |k, i| {
        Ok(idents(k.host.hook_plan(i.root.0, &i.target, &i.hook)?.iter().copied()))
    };
    /// Connect a scope listener to a source.
    Listen = "listen" [feature = "subscriptions"] {
        scope: Handle,
        previous: Option<Handle>,
        name: String,
        source: Handle,
        sizing: Option<Handle>,
        operation: Handle,
        max_pending: u32,
        max_bytes: Option<u32>,
        coalesce: Option<bool>,
    } -> Ident => |k, i| {
        k.host.listen(
            i.scope.0,
            i.previous.map(|previous| previous.0),
            i.name,
            i.source.0,
            i.sizing.map_or(0, |sizing| sizing.0),
            i.operation.0,
            i.max_pending as usize,
            i.max_bytes.map(|bytes| bytes as usize),
            i.coalesce.unwrap_or(false),
        )
        .map(Ident)
    };
    /// Subscribe an owner to a source.
    Subscribe = "subscribe" [feature = "subscriptions"] {
        owner: OwnerInput,
        source: Handle,
        operation: Handle,
        max_pending: u32,
        max_bytes: Option<u32>,
        coalesce: Option<bool>,
    } -> Ident => |k, i| {
        k.host.subscribe(
            i.owner.resolve()?,
            i.source.0,
            i.operation.0,
            i.max_pending as usize,
            i.max_bytes.map(|bytes| bytes as usize),
            i.coalesce.unwrap_or(false),
        )
        .map(Ident)
    };
    /// Queue a delivery.
    Enqueue = "enqueue" [feature = "subscriptions"] {
        subscription: Handle,
        payload: Handle,
        bytes: Option<u32>,
    } -> () => |k, i| {
        k.host.enqueue(
            i.subscription.0,
            core::Delivery {
                payload: i.payload.0,
                bytes: i.bytes.unwrap_or(0) as usize,
            },
        )
    };
    /// Take the next queued delivery payload.
    NextDelivery = "next-delivery" [feature = "subscriptions"] { subscription: Handle } -> Option<Ident> => |k, i| {
        Ok(k.host.next_delivery(i.subscription.0)?.map(|delivery| Ident(delivery.payload)))
    };
    /// Finish the current delivery.
    FinishDelivery = "finish-delivery" [feature = "subscriptions"] { subscription: Handle } -> () => |k, i| {
        k.host.finish_delivery(i.subscription.0)
    };
    /// Remove a subscription.
    Unsubscribe = "unsubscribe" [feature = "subscriptions"] { subscription: Handle } -> () => |k, i| {
        k.host.unsubscribe(i.subscription.0)
    };
}

/// Use the root frame when no context frame is supplied.
fn frame(context: Option<Handle>) -> Id {
    context.map_or(0, |context| context.0)
}

/// Drain pending actions in kernel order.
fn drain(kernel: &Kernel) -> Vec<Action> {
    kernel.host.drain().into_iter().map(Action::from).collect()
}

/// Reject operations absent from this build's protocol implementation.
pub(crate) fn unavailable(name: &str) -> Error {
    Error::new(
        Code::Unsupported,
        Reason::Operation,
        format!("operation {name} is unavailable in this build"),
    )
}
