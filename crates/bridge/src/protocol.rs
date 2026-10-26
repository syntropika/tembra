//! Closed names used only at adapter serialization seams.
macro_rules! names {
    ($(#[$meta:meta])* $name:ident { $($variant:ident => $wire:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub enum $name { $($variant),+ }
        impl $name {
            /// Return the stable wire name for this variant.
            pub const fn as_str(self) -> &'static str { match self { $(Self::$variant => $wire),+ } }
            /// Reject wire names outside this protocol enum.
            pub fn parse(value: &str) -> Option<Self> {
                match value { $($wire => Some(Self::$variant),)+ _ => None }
            }
        }
    };
}

names! {
    /// Stable wire names; unknown input is rejected before dispatch.
    KernelOp {
        ContextDerive => "context-derive",
        ContextValue => "context-value",
        ContextStats => "context-stats",
        Allocate => "allocate",
        ScopeRegister => "scope-register",
        ScopeExists => "scope-exists",
        ScopesForOwner => "scopes-for-owner",
        ScopesForFacet => "scopes-for-facet",
        ScopeAddResource => "scope-add-resource",
        ScopeActivate => "scope-activate",
        ScopesCleanup => "scopes-cleanup",
        ScopesReloadRegistered => "scopes-reload-registered",
        ScopesReload => "scopes-reload",
        ProgramRun => "program-run",
        ProgramNext => "program-next",
        ProgramCompleteStep => "program-complete-step",
        ProgramCommit => "program-commit",
        ProgramAbort => "program-abort",
        ProgramForget => "program-forget",
        Features => "features",
        Prepare => "prepare",
        Gate => "gate",
        GateFacets => "gate-facets",
        Commit => "commit",
        Abort => "abort",
        Facets => "facets",
        Facet => "facet",
        FacetNamed => "facet-named",
        NextDeadline => "next-deadline",
        Address => "address",
        Acquire => "acquire",
        Ready => "ready",
        Dependency => "dependency",
        Undepend => "undepend",
        Fail => "fail",
        Status => "status",
        Release => "release",
        Forget => "forget",
        Root => "root",
        CleanupRoot => "cleanup-root",
        RootPrincipal => "root-principal",
        CheckContext => "check-context",
        CheckRoot => "check-root",
        EndRoot => "end-root",
        CancelRoot => "cancel-root",
        BeginCall => "begin-call",
        BeginOperation => "begin-operation",
        FinishCall => "finish-call",
        Invocation => "invocation",
        StateInit => "state-init",
        Snapshot => "snapshot",
        StateAdopt => "state-adopt",
        Task => "task",
        TaskRoot => "task-root",
        StopTask => "stop-task",
        FinishTask => "finish-task",
        Resource => "resource",
        ResourceOpened => "resource-opened",
        ResourceValue => "resource-value",
        CloseResource => "close-resource",
        ResourceClosed => "resource-closed",
        Disposed => "disposed",
        Tick => "tick",
        NextWake => "next-wake",
        NextIdle => "next-idle",
        Close => "close",
        FinishClose => "finish-close",
        Diagnostics => "diagnostics",
        Drain => "drain",
        Restrict => "restrict",
        Contribute => "contribute",
        HookPlan => "hook-plan",
        Listen => "listen",
        Subscribe => "subscribe",
        Enqueue => "enqueue",
        NextDelivery => "next-delivery",
        FinishDelivery => "finish-delivery",
        Unsubscribe => "unsubscribe",
    }
}

names! {
    /// Stable wire names; unknown input is rejected before dispatch.
    EffectType {
        Stop => "stop",
        Open => "open",
        Close => "close",
        Restore => "restore",
        Prepare => "prepare",
        Validate => "validate",
        Initialize => "initialize",
        Migrate => "migrate",
        Adopt => "adopt",
        Start => "start",
        Drop => "drop",
    }
}

names! {
    /// Stable wire names; unknown input is rejected before dispatch.
    ActionType {
        Execute => "execute",
        CancelEffects => "cancel-effects",
        ProgramPrepared => "program-prepared",
        ProgramComplete => "program-complete",
        Dispose => "dispose",
        CloseResource => "close-resource",
        DropValue => "drop-value",
        CancelRoot => "cancel-root",
        CancelTask => "cancel-task",
        RetireFacet => "retire-facet",
        State => "state",
        SubscriptionFailed => "subscription-failed",
        DetachSubscription => "detach-subscription",
        Deliver => "deliver",
    }
}

names! {
    /// Stable wire names; unknown input is rejected before dispatch.
    StepType {
        Execute => "execute",
        Wait => "wait",
        Prepared => "prepared",
        Complete => "complete",
        Failed => "failed",
    }
}

names! {
    /// Stable wire names; unknown input is rejected before dispatch.
    NativeOp {
        Activate => "activate",
        Dispose => "dispose",
        CancelEffect => "cancel-effect",
        Effect => "effect",
        Invoke => "invoke",
    }
}
