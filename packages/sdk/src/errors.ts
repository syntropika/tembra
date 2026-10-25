import { Code, Reason } from "./identifiers.ts";

/** Shared error shape, also used by Rust and transport envelopes. Facets may supply custom reasons. */
export interface ErrorDescriptor {
  readonly code: Code;
  readonly reason: string;
  readonly message: string;
}

function define(code: Code, reason: string, message: string): Readonly<ErrorDescriptor> {
  return Object.freeze({ code, reason, message });
}

/** Built-in errors keep their identity and default message together. */
export const Errors = Object.freeze({
  Cancelled: Object.freeze({
    EngineEffect: define(Code.Cancelled, Reason.EngineEffect, "The engine effect was cancelled"),
    Request: define(Code.Cancelled, Reason.Request, "The request was cancelled"),
    Resource: define(Code.Cancelled, Reason.Resource, "Resource opening was cancelled"),
    Root: define(Code.Cancelled, Reason.Root, "The operation was cancelled"),
    Task: define(Code.Cancelled, Reason.Task, "The task was cancelled"),
  }),
  Closed: Object.freeze({
    Host: define(Code.Closed, Reason.Host, "The host is closing"),
  }),
  Conflict: Object.freeze({
    AlreadyCommitted: define(
      Code.Conflict,
      Reason.AlreadyCommitted,
      "The replacement is already committed",
    ),
    Codec: define(Code.Conflict, Reason.Codec, "The Rust and JS host codecs must agree"),
    PrepareDeadline: define(
      Code.Conflict,
      Reason.PrepareDeadline,
      "The preparation deadline expired",
    ),
    ReloadRunning: define(Code.Conflict, Reason.ReloadRunning, "A replacement is already running"),
    ReloadTicket: define(
      Code.Conflict,
      Reason.ReloadTicket,
      "The replacement is no longer prepared",
    ),
  }),
  Exhausted: Object.freeze({
    FrameSize: define(
      Code.Exhausted,
      Reason.FrameSize,
      "The frame exceeds the configured size limit",
    ),
    PendingRequests: define(
      Code.Exhausted,
      Reason.PendingRequests,
      "The pending request limit was reached",
    ),
    RemoteReferences: define(
      Code.Exhausted,
      Reason.RemoteReferences,
      "The remote reference limit was reached",
    ),
    RequestIds: define(Code.Exhausted, Reason.RequestIds, "Request identities are exhausted"),
    SubscriptionQueue: define(
      Code.Exhausted,
      Reason.SubscriptionQueue,
      "The subscription queue is full",
    ),
  }),
  Expired: Object.freeze({
    Next: define(Code.Expired, Reason.Next, "Continuation scope has ended"),
  }),
  Forbidden: Object.freeze({
    Context: define(Code.Forbidden, Reason.Context, "An issued context from this host is required"),
    HostResource: define(Code.Forbidden, Reason.HostResource, "The host resource was not declared"),
    Method: define(Code.Forbidden, Reason.Method, "The method is not allowed by this reference"),
    Opaque: define(
      Code.Forbidden,
      Reason.Opaque,
      "The resource is not available as an opaque local value",
    ),
    Service: define(Code.Forbidden, Reason.Service, "Access to this service is forbidden"),
    StateOwner: define(
      Code.Forbidden,
      Reason.StateOwner,
      "Only the owning facet may change this state",
    ),
    UndeclaredDependency: define(
      Code.Forbidden,
      Reason.UndeclaredDependency,
      "The dependency was not declared",
    ),
    UndeclaredService: define(
      Code.Forbidden,
      Reason.UndeclaredService,
      "The service was not declared as a dependency",
    ),
  }),
  Gone: Object.freeze({
    Connection: define(Code.Gone, Reason.Connection, "The connection is closed"),
    Contribution: define(Code.Gone, Reason.Contribution, "The contribution is no longer available"),
    Implementation: define(
      Code.Gone,
      Reason.Implementation,
      "The implementation is no longer available",
    ),
    ListenerCallback: define(
      Code.Gone,
      Reason.ListenerCallback,
      "The listener callback is no longer available",
    ),
    Reference: define(Code.Gone, Reason.Reference, "The reference is no longer available"),
    Resource: define(Code.Gone, Reason.Resource, "The resource is no longer available"),
    Scope: define(Code.Gone, Reason.Scope, "The declaration scope is no longer available"),
    State: define(Code.Gone, Reason.State, "State owner is gone"),
  }),
  Invalid: Object.freeze({
    Address: define(Code.Invalid, Reason.Address, "Invalid address"),
    AsyncFactory: define(Code.Invalid, Reason.AsyncFactory, "Factories must be synchronous"),
    AsyncState: define(
      Code.Invalid,
      Reason.AsyncState,
      "State updates and migrations must be synchronous",
    ),
    CoordinationLimits: define(
      Code.Invalid,
      Reason.CoordinationLimits,
      "Invalid coordination limits",
    ),
    DependencyService: define(Code.Invalid, Reason.DependencyService, "Invalid dependency service"),
    DuplicateListener: define(Code.Invalid, Reason.DuplicateListener, "Invalid duplicate listener"),
    DuplicateResource: define(Code.Invalid, Reason.DuplicateResource, "Invalid duplicate resource"),
    EventSource: define(Code.Invalid, Reason.EventSource, "Invalid event source"),
    Facet: define(Code.Invalid, Reason.Facet, "A facet needs a name and positive API version"),
    FacetGone: define(Code.Invalid, Reason.FacetGone, "Invalid facet gone"),
    FacetInstalled: define(
      Code.Invalid,
      Reason.FacetInstalled,
      "Use reload to replace an installed facet",
    ),
    Factory: define(Code.Invalid, Reason.Factory, "Invalid factory"),
    FactoryExpired: define(Code.Invalid, Reason.FactoryExpired, "Declarations must be synchronous"),
    Hook: define(Code.Invalid, Reason.Hook, "Hooks need a run function"),
    HookDependency: define(Code.Invalid, Reason.HookDependency, "Invalid hook dependency"),
    HooksDisabled: define(Code.Invalid, Reason.HooksDisabled, "Invalid hooks disabled"),
    JsonAccessor: define(Code.Invalid, Reason.JsonAccessor, "Invalid json accessor"),
    JsonCycle: define(Code.Invalid, Reason.JsonCycle, "Invalid json cycle"),
    JsonNumber: define(
      Code.Invalid,
      Reason.JsonNumber,
      "Use BigInt for integers outside the safe Number range",
    ),
    JsonSymbol: define(Code.Invalid, Reason.JsonSymbol, "Invalid json symbol"),
    JsonTag: define(Code.Invalid, Reason.JsonTag, "Unknown or malformed portable type tag"),
    JsonType: define(Code.Invalid, Reason.JsonType, "Supply a codec for custom object types"),
    JsonValue: define(
      Code.Invalid,
      Reason.JsonValue,
      "This JSON codec requires explicit portable values",
    ),
    Method: define(Code.Invalid, Reason.Method, "Methods must be functions"),
    NextUsed: define(Code.Invalid, Reason.NextUsed, "Invalid next used"),
    Priority: define(Code.Invalid, Reason.Priority, "Invalid priority"),
    Reference: define(Code.Invalid, Reason.Reference, "An issued local reference is required"),
    Resource: define(Code.Invalid, Reason.Resource, "Invalid resource"),
    ResourceHost: define(
      Code.Invalid,
      Reason.ResourceHost,
      "Resource dependencies must belong to the same host",
    ),
    ResourceInFactory: define(
      Code.Invalid,
      Reason.ResourceInFactory,
      "Resources open after declaration",
    ),
    RpcFrame: define(Code.Invalid, Reason.RpcFrame, "Invalid rpc frame"),
    RpcOperation: define(Code.Invalid, Reason.RpcOperation, "Invalid rpc operation"),
    RpcRequest: define(Code.Invalid, Reason.RpcRequest, "Invalid rpc request"),
    Service: define(Code.Invalid, Reason.Service, "Invalid service"),
    ServiceGone: define(Code.Invalid, Reason.ServiceGone, "Invalid service gone"),
    State: define(Code.Invalid, Reason.State, "Service does not expose state"),
    StateCycle: define(Code.Invalid, Reason.StateCycle, "State cannot contain cycles"),
    StateValidation: define(
      Code.Invalid,
      Reason.StateValidation,
      "The facet rejected the state candidate",
    ),
    StateValue: define(Code.Invalid, Reason.StateValue, "State cannot contain accessors"),
    StateVersion: define(Code.Invalid, Reason.StateVersion, "Invalid state version"),
    SubscriptionsDisabled: define(
      Code.Invalid,
      Reason.SubscriptionsDisabled,
      "Invalid subscriptions disabled",
    ),
    SubscriptionSize: define(
      Code.Invalid,
      Reason.SubscriptionSize,
      "A local byte budget requires an explicit sizeOf function",
    ),
    Task: define(Code.Invalid, Reason.Task, "Invalid task"),
    Utf8: define(Code.Invalid, Reason.Utf8, "Invalid utf8"),
    ValueGone: define(Code.Invalid, Reason.ValueGone, "Invalid value gone"),
    ValuesExhausted: define(Code.Invalid, Reason.ValuesExhausted, "Invalid values exhausted"),
  }),
  Released: Object.freeze({
    Reference: define(Code.Released, Reason.Reference, "The reference has been released"),
  }),
  Unavailable: Object.freeze({
    Callback: define(Code.Unavailable, Reason.Callback, "A callback failed"),
    FacetScope: define(Code.Unavailable, Reason.FacetScope, "The facet scope is unavailable"),
    NotReady: define(Code.Unavailable, Reason.NotReady, "The object is not ready"),
    Route: define(Code.Unavailable, Reason.Route, "The route is unavailable"),
  }),
  Unsupported: Object.freeze({
    EngineEffect: define(
      Code.Unsupported,
      Reason.EngineEffect,
      "The engine effect is not supported by this adapter",
    ),
    RemoteOpaque: define(
      Code.Unsupported,
      Reason.RemoteOpaque,
      "Opaque resources are local to their language environment",
    ),
    Telemetry: define(Code.Unsupported, Reason.Telemetry, "Telemetry is disabled in this kernel"),
    Views: define(Code.Unsupported, Reason.Views, "Reference views are disabled in this kernel"),
  }),
});
