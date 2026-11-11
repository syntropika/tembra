import "./builtins.ts";

// Host state and authority never live in these guest-side proxy objects.
(() => {
  const environments = new Map<number, unknown>();
  /** The service alias of each declared environment; `null` names an Orb root. */
  const environmentServices = new Map<number, string | null>();
  /**
   * Exported functions by the id the host selects them with: the contract's export slot. Ids are
   * local to this revision and grant nothing; each command's role and environment are checked.
   */
  const functions = new Map<
    number,
    { readonly contract: string; readonly service: string; readonly method: string }
  >();
  for (const exported of globalThis.__tembraExports!) {
    for (const [id, method] of exported.functions) {
      if (functions.has(id)) throw new Error("Duplicate exported function id");
      functions.set(
        id,
        Object.freeze({ contract: exported.interface, service: exported.service, method }),
      );
    }
  }
  delete globalThis.__tembraExports;
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
  const sweepStandard = globalThis.__tembraSweepStandard!;
  delete globalThis.__tembraSweepStandard;
  const streamResult = globalThis.__tembraStreamResult!;
  delete globalThis.__tembraStreamResult;
  const streamImport = globalThis.__tembraStreamImport!;
  delete globalThis.__tembraStreamImport;
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

  /** Encode an operation on a handle issued by the `scope` environment's host table. */
  function scoped<
    O extends
      | "state.read"
      | "state.update"
      | "reference.release"
      | "reference.status"
      | "reference.state"
      | "resource.read",
  >(scope: number, operation: O, input: Omit<HostRequests[O], "scope">): any {
    return JSON.parse(__hostSync(JSON.stringify({ operation, scope, ...(input as object) })));
  }

  /** Normalize a key: the host protocol carries only lists, a string names a one-segment key. */
  function keyList(key: string | readonly string[]): string[] {
    if (typeof key === "string") return [key];
    if (!Array.isArray(key)) throw new Error("A reference key is a string or a list of strings");
    return [...key];
  }

  /**
   * Proxy one host reference handle of a WIT import. Declared and `Context.get` handles share it;
   * the host decides which callbacks may still use the handle.
   */
  function reference(scope: number, id: number, service: string) {
    const state = stateValues.readable((known) =>
      scoped(scope, "reference.state", known === undefined ? { id } : { id, known }),
    );
    return new Proxy(Object.create(null), {
      get(_target, method) {
        if (method === "then") return undefined;
        if (method === "release") return () => scoped(scope, "reference.release", { id });
        if (method === "ready") return call("reference.ready", { scope, id });
        if (method === "status") return scoped(scope, "reference.status", { id });
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
          const request = {
            scope,
            id,
            method,
            args: wire.importArguments(service, method, args),
          };
          // A whole `stream<u8>` result arrives as a host-issued reader on its own carrier.
          if (wire.importStream(service, method))
            return streamImport(JSON.stringify({ operation: "reference.call", ...request }));
          return call("reference.call", request).then((value) =>
            wire.importResult(service, method, value),
          );
        };
      },
    });
  }

  /** Create a declaration proxy whose operations are checked by the outer host. */
  function declaration(owner: number) {
    /** Avoid retaining callbacks after the factory's declaration pass has ended. */
    function checkFactory() {
      if (declaring !== owner) throw new Error("Declaration factory has ended");
    }
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
          scoped(owner, "state.read", known === undefined ? { id } : { id, known }),
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
            return view.adopt(scoped(owner, "state.update", { id, value }));
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
        const id = sync("reference.declare", { service, key: keyList(key) });
        return reference(owner, id, service);
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
          await options.close?.(value, context(owner));
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
            return values.get(scoped(owner, "resource.read", { id }));
          },
        });
      },
    });
  }

  /**
   * Expose operations on the currently admitted context, never its numeric kernel handle.
   * `environment` is the host environment running the callback; its table receives the handles
   * acquired through this context.
   */
  function context(environment: number) {
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
      /**
       * Acquire a selected WIT Orb import by key for this context's root and principal. The host
       * resolves the provider and its installed requirement, waits for readiness and releases
       * the claim when the root ends; the handle is refused in every other root.
       */
      async get(service: string, key: string | readonly string[], options?: unknown) {
        checkLifetime();
        if (options !== undefined)
          throw new Error("Acquisition options are unsupported by the isolated value profile");
        const id = await call("reference.acquire", { service, key: keyList(key) });
        return reference(environment, id, service);
      },
    });
  }

  /**
   * Resolve a host-selected function id. A service method runs only in an environment of its own
   * service, a contribution only in a declared Orb root; unknown ids and released or foreign
   * environments are refused.
   */
  function exported(id: unknown, environment: unknown, contribution: boolean) {
    const target = functions.get(id as number);
    if (!target) throw new Error("Unknown exported function");
    const service = environmentServices.get(environment as number);
    if (service === undefined) throw new Error("Unknown or released interpreter environment");
    if (contribution ? service !== null : service !== target.service)
      throw new Error("The function does not belong to this environment");
    return target;
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
    environmentServices.set(request.environment, request.service);
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
        case "invoke": {
          const { service, method } = exported(request.function, request.environment, false);
          const methods = orb.services[service]?.methods;
          if (!methods || typeof methods[method] !== "function")
            throw new Error("Unknown service method");
          const environment = environments.get(request.environment);
          if (wire.streamResult(service, method)) {
            // The result leaves as a stream; this callback lasts until it was produced.
            await streamResult(
              await methods[method]!(
                environment,
                context(request.environment),
                ...wire.arguments(service, method, request.args),
              ),
            );
            break;
          }
          result = await wire.invoke(service, method, () =>
            methods[method]!(
              environment,
              context(request.environment),
              ...wire.arguments(service, method, request.args),
            ),
          );
          break;
        }
        case "contribute": {
          const { contract, service, method } = exported(
            request.function,
            request.environment,
            true,
          );
          const contribution = orb.contributes?.[contract];
          const handle = typeof contribution === "function" ? contribution : contribution?.handle;
          if (!handle) throw new Error("Unknown contribution callback");
          // Contributions run in the Orb root environment, like that environment's services.
          result = await wire.invoke(service, method, () =>
            handle(
              environments.get(request.environment),
              context(request.environment),
              wire.arguments(service, method, request.args)[0],
            ),
          );
          break;
        }
        case "ingress": {
          // The host-local HTTP handler: an ordinary service method whose argument is the
          // host-issued request handle. The callback lasts until the response was delivered.
          if (environmentServices.get(request.environment) !== request.service)
            throw new Error("The HTTP handler does not belong to this environment");
          const handle = orb.services[request.service]?.methods[request.method];
          if (typeof handle !== "function") throw new Error("Unknown HTTP handler service");
          const environment = environments.get(request.environment);
          await globalThis.__tembraIngress!.serve(request.request, (incoming) =>
            handle(environment, context(request.environment), incoming),
          );
          break;
        }
        case "callback":
          result = await callbacks.get(request.callback)!(request.value);
          break;
        case "release":
          environments.delete(request.environment);
          environmentServices.delete(request.environment);
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
  });
})();
