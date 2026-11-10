import "./builtins.ts";

// Host state and authority never live in these guest-side proxy objects.
(() => {
  const environments = new Map<number, unknown>();
  const callbacks = new Map<number, GuestCallback>();
  const values = new Map<number, unknown>();
  const owners = new Map<number, number | null>();
  let next = 1;
  let declaring: number | null = null;
  let activeCallback: symbol | null = null;
  const contextMarker = Symbol("tembra-context");
  const resourceMarker = Symbol("tembra-resource");
  const wire = globalThis.__tembraValues;
  const stateValues = globalThis.__tembraState;
  const imports = new Set(globalThis.__tembraContract.imports.map((item) => item.contract));
  const sweepStandard = globalThis.__tembraSweepStandard!;
  delete globalThis.__tembraSweepStandard;
  const writeLog = globalThis.__writeLog!;
  delete globalThis.__writeLog;
  Object.defineProperty(globalThis, "console", {
    value: Object.freeze(
      Object.fromEntries(
        ["log", "debug", "info", "warn", "error"].map((level) => [
          level,
          (...args: unknown[]) => {
            writeLog(
              level,
              args.map((value) => (typeof value === "string" ? value : String(value))).join(" "),
            );
          },
        ]),
      ),
    ),
  });

  /** Encode a portable operation for the current host-issued scope. */
  function sync<O extends keyof HostRequests>(
    operation: O,
    ...[input]: HostInput<HostRequests[O]>
  ): any {
    return JSON.parse(__hostSync(JSON.stringify({ operation, ...(input as object | undefined) })));
  }

  /** Await a host operation without accepting guest-supplied context identities. */
  function call<O extends keyof AsyncHostRequests>(
    operation: O,
    input: AsyncHostRequests[O],
  ): Promise<any> {
    try {
      return __hostAsync(JSON.stringify({ operation, ...(input as object) })).then((value) =>
        JSON.parse(value),
      );
    } catch (error) {
      // Value-only operations promise a result even when admission refuses them immediately.
      return Promise.reject(error);
    }
  }

  /** Retain a guest callback without transferring executable code to the host. */
  function retain(callback: GuestCallback): number {
    const id = next++;
    callbacks.set(id, callback);
    owners.set(id, declaring);
    return id;
  }

  /** Create a declaration proxy whose operations are checked by the outer host. */
  function declaration(owner: number) {
    /** Avoid retaining callbacks after the factory's declaration pass has ended. */
    function checkFactory() {
      if (declaring !== owner) throw new Error("Declaration factory has ended");
    }
    /** Bind every issued proxy to its actual environment identity. */
    const scoped = <
      O extends
        | "state.read"
        | "state.update"
        | "reference.release"
        | "reference.status"
        | "reference.state"
        | "resource.read",
    >(
      operation: O,
      input: Omit<HostRequests[O], "scope">,
    ): any =>
      JSON.parse(__hostSync(JSON.stringify({ operation, scope: owner, ...(input as object) })));
    return Object.freeze({
      get key() {
        return sync("address").key;
      },
      get principal() {
        return sync("address").principal;
      },
      get orb() {
        return environments.get(sync("orb"));
      },
      state(
        initial: unknown,
        options: { version?: number; migrate?: unknown; validate?(value: Json): unknown } = {},
      ) {
        checkFactory();
        if (options.migrate) throw new Error("State migration is not implemented by this adapter");
        /** Reject asynchronous updates and boolean validator refusals before adoption. */
        const validate = (value: any): Json => {
          if (value?.then) throw new Error("State must be synchronous");
          value = stateValues.protect(value);
          const result = options.validate?.(value);
          if (result !== undefined && result !== true) throw new Error("State validation failed");
          return value;
        };
        initial = validate(initial);
        const check = options.validate
          ? retain((value: unknown) => {
              validate(value);
              return null;
            })
          : null;
        const id = sync("state.declare", {
          initial,
          version: options.version ?? 1,
          validate: check,
        });
        const view = stateValues.readable((known) =>
          scoped("state.read", known === undefined ? { id } : { id, known }),
        );
        if (check !== null) validate(view.value);
        return Object.freeze({
          get value() {
            return view.value;
          },
          select(compute: (value: Json) => Json) {
            return view.select(compute);
          },
          subscribe() {
            return view.subscribe();
          },
          update(context: { check(): void }, change: (value: Json) => unknown) {
            context.check();
            const value = validate(change(view.value));
            return view.adopt(scoped("state.update", { id, value }));
          },
        });
      },
      hook(name: string) {
        checkFactory();
        const exported = globalThis.__tembraContract.exports.find((item) => {
          const alias = item.contract?.split("/").at(-1)?.split("@")[0];
          return alias && globalThis.__tembraContract.javascriptNames[alias] === name;
        });
        const method = exported && Object.keys(exported.functions)[0];
        if (!method) throw new Error("Unknown WIT hook");
        const spelling = globalThis.__tembraContract.javascriptNames[method]!;
        const id = sync("hook.declare", { name });
        return Object.freeze({
          async run(ctx: GuestObject, input: unknown) {
            if (ctx?.[contextMarker] !== true)
              throw new Error("Hook calls require an issued context");
            ctx.check();
            const args = wire.exportArguments(name, spelling, [input]);
            const result = await call("hook.run", { scope: owner, id, value: args[0] });
            return wire.exportResult(name, spelling, result);
          },
        });
      },
      get(service: string, key: string | readonly string[] = ["main"], options?: unknown) {
        checkFactory();
        if (options !== undefined)
          throw new Error("Acquisition options are unsupported by the isolated value profile");
        // The host protocol carries only key lists; a single string names a one-segment key.
        const id = sync("reference.declare", {
          service,
          key: typeof key === "string" ? [key] : [...key],
        });
        const state = stateValues.readable((known) =>
          scoped("reference.state", known === undefined ? { id } : { id, known }),
        );
        return new Proxy(Object.create(null), {
          get(_target, method) {
            if (method === "then") return undefined;
            if (method === "release") return () => scoped("reference.release", { id });
            if (method === "ready") return call("reference.ready", { scope: owner, id });
            if (method === "status") return scoped("reference.status", { id });
            if (method === "state") return state;
            if (method === "opaque")
              return () => {
                throw new Error("Cross-heap opaque values require a resource adapter");
              };
            if (!wire.hasImportMethod(service, method))
              throw new Error(`Unknown WIT import method ${service}#${String(method)}`);
            if (typeof method !== "string") return undefined;
            return (...args: any[]) => {
              if (args.at(-1)?.[contextMarker] === true) args.pop();
              return call("reference.call", {
                scope: owner,
                id,
                method,
                args: wire.importArguments(service, method, args),
              }).then((value) => wire.importResult(service, method, value));
            };
          },
        });
      },
      resource(
        name: string,
        options?: {
          version?: number;
          exclusive?: boolean;
          opaque?: boolean;
          deps?: GuestObject[];
          open: GuestCallback;
          close?: GuestCallback;
        },
      ) {
        checkFactory();
        if (options === undefined)
          throw new Error(
            "Named host resources require a cross-heap value adapter; supply an opener",
          );
        if (options.exclusive || options.opaque)
          throw new Error("This adapter does not yet support these resource options");
        const dependencies = (options.deps ?? []).map((resource) => {
          const key = resource?.[resourceMarker];
          if (!key) throw new Error("Resource dependencies must be issued resource handles");
          return key;
        });
        const open = retain(async () => {
          const callback = activeCallback;
          const value = await options.open(
            Object.freeze({
              get aborted() {
                return activeCallback !== callback || sync("signal.cancelled");
              },
              throwIfAborted() {
                if (activeCallback !== callback || sync("signal.cancelled"))
                  throw new Error("Operation cancelled");
              },
            }),
          );
          const id = next++;
          values.set(id, value);
          owners.set(id, owner);
          return id;
        });
        const close = retain(async (id: number) => {
          const value = values.get(id);
          await options.close?.(value, context());
          values.delete(id);
          owners.delete(id);
        });
        const id = sync("resource.declare", {
          name,
          version: options.version ?? 1,
          open,
          close,
          dependencies,
        });
        return Object.freeze({
          [resourceMarker]: Object.freeze({ scope: owner, id }),
          version: options.version ?? 1,
          get value() {
            return values.get(scoped("resource.read", { id }));
          },
        });
      },
    });
  }

  /** Expose operations on the currently admitted context, never its numeric kernel handle. */
  function context() {
    const callback = activeCallback;
    /** A retained guest proxy cannot inherit a later callback's native context. */
    function checkLifetime() {
      if (callback === null || activeCallback !== callback)
        throw new Error("Context callback has ended");
    }
    return Object.freeze({
      [contextMarker]: true,
      get principal() {
        checkLifetime();
        return sync("context.principal");
      },
      check() {
        checkLifetime();
        sync("context.check");
      },
      cancel() {
        checkLifetime();
        sync("context.cancel");
      },
    });
  }

  /** Describe callbacks without serializing or executing service factories. */
  globalThis.__tembraDescribe = () => {
    const orb = globalThis.__tembraOrb;
    if (!orb || typeof orb !== "object") throw new Error("Bundle must export a default Orb");
    for (const hook of Object.values(orb.hooks ?? {})) {
      if (!["series", "waterfall", "bail"].includes(hook.composition))
        throw new Error("Portable hooks require a supported composition");
    }
    for (const contribution of Object.values(orb.contributes ?? {})) {
      if (
        typeof (typeof contribution === "function" ? contribution : contribution.handle) !==
        "function"
      )
        throw new Error("A contribution requires an executable callback");
    }
    return JSON.stringify({
      name: orb.name,
      apiVersion: orb.apiVersion,
      hooks: Object.fromEntries(
        Object.entries(orb.hooks ?? {}).map(([name, hook]) => [name, hook.composition]),
      ),
      contributes: Object.fromEntries(
        Object.entries(orb.contributes ?? {}).map(([binding, contribution]) => [
          binding,
          typeof contribution === "function" ? 0 : (contribution.priority ?? 0),
        ]),
      ),
      services: Object.fromEntries(
        Object.entries(orb.services ?? {}).map(([name, value]) => [
          name,
          {
            methods: Object.keys(value.methods),
            lifetime: value.lifetime,
            linger: value.linger,
            access: !!value.access,
          },
        ]),
      ),
    });
  };

  /** Run only a synchronous factory while native host declaration authority is available. */
  globalThis.__tembraDeclare = (packet) => {
    const request = JSON.parse(packet);
    if (request.kind === "validate") {
      const result = callbacks.get(request.callback)!(request.value);
      if (result?.then) throw new Error("State validators must be synchronous");
      return JSON.stringify(result === undefined ? null : result);
    }
    if (request.kind !== "create")
      throw new Error("Only factory declarations may use this entrypoint");
    const orb = globalThis.__tembraOrb;
    const factory = request.service === null ? orb.create : orb.services[request.service]?.create;
    declaring = request.environment;
    let value;
    try {
      value = factory?.(declaration(request.environment)) ?? {};
    } finally {
      declaring = null;
    }
    if (typeof value?.then === "function") throw new Error("Factories must be synchronous");
    environments.set(request.environment, value);
    return "null";
  };

  /** Execute one bounded host-selected callback against the retained Orb revision. */
  globalThis.__tembraDispatch = async (packet) => {
    const callback = Symbol("callback");
    activeCallback = callback;
    try {
      sweepStandard();
      const request = JSON.parse(packet);
      const orb = globalThis.__tembraOrb;
      let result;
      switch (request.kind) {
        case "invoke":
          result = await wire.invoke(request.service, request.method, () =>
            orb.services[request.service]!.methods[request.method]!(
              environments.get(request.environment),
              context(),
              ...wire.arguments(request.service, request.method, request.args),
            ),
          );
          break;
        case "contribute": {
          const contribution = orb.contributes?.[request.binding];
          const handle = typeof contribution === "function" ? contribution : contribution?.handle;
          if (!handle) throw new Error("Unknown contribution callback");
          // Contributions run in the Orb root environment, like that environment's services.
          result = await wire.invoke(request.service, request.method, () =>
            handle(
              environments.get(request.environment),
              context(),
              wire.arguments(request.service, request.method, request.args)[0],
            ),
          );
          break;
        }
        case "callback":
          result = await callbacks.get(request.callback)!(request.value);
          break;
        case "release":
          environments.delete(request.environment);
          for (const [id, owner] of owners) {
            if (owner !== request.environment) continue;
            callbacks.delete(id);
            values.delete(id);
            owners.delete(id);
          }
          break;
        default:
          throw new Error("Unknown interpreter command");
      }
      return JSON.stringify(result === undefined ? null : result);
    } finally {
      // Callback end: also close this callback's unfinished standard input pumps.
      sweepStandard(true);
      if (activeCallback === callback) activeCallback = null;
    }
  };

  /** Largest byte chunk carried by one transport read or write. */
  const TRANSPORT_CHUNK_BYTES = 16 * 1024;

  /**
   * Wrap a host-issued stream identity. The identity selects only this Orb's streams of the
   * active callback; the host closes every stream when that callback completes.
   */
  function transportStream(id: number): TransportStream {
    return Object.freeze({
      async read() {
        const bytes: number[] | null = await call("transport.read", { id });
        return bytes === null ? null : Uint8Array.from(bytes);
      },
      async write(bytes: Uint8Array) {
        if (!(bytes instanceof Uint8Array))
          throw new TypeError("Transport writes require a Uint8Array");
        if (bytes.length > TRANSPORT_CHUNK_BYTES)
          throw new RangeError("Transport writes are limited to 16 KiB per call");
        await call("transport.write", { id, bytes: Array.from(bytes) });
      },
      async close() {
        await call("transport.close", { id });
      },
    });
  }

  globalThis.__tembraAbandon = () => {
    activeCallback = null;
    // An abandoned callback's pumps end too; the host closes any writer this cannot reach.
    try {
      sweepStandard(true);
    } catch {
      // Cleanup never turns abandonment into a new failure.
    }
  };

  globalThis.tembra = Object.freeze({
    wait(milliseconds: number) {
      return call("wait", { milliseconds });
    },
    ...(imports.has("wasi:sockets/types@0.3.0")
      ? {
          transport: Object.freeze({
            /** Open one incremental byte stream to an exactly granted literal address. */
            async connect(ip: string, port: number): Promise<TransportStream> {
              return transportStream(await call("transport.connect", { ip, port }));
            },
          }),
        }
      : {}),
  });
})();
