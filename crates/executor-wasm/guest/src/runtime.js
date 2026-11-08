// Host state and authority never live in these guest-side proxy objects.
(() => {
  const environments = new Map();
  const callbacks = new Map();
  const values = new Map();
  const owners = new Map();
  let next = 1;
  let declaring = null;
  let activeCallback = null;
  const contextMarker = Symbol("tembra-context");
  const resourceMarker = Symbol("tembra-resource");
  const wire = globalThis.__tembraValues;
  const stateValues = globalThis.__tembraState;
  const readText = globalThis.__readText;
  const writeText = globalThis.__writeText;
  const exchange = globalThis.__tcpExchange;
  delete globalThis.__readText;
  delete globalThis.__writeText;
  delete globalThis.__tcpExchange;
  const imports = new Set(globalThis.__tembraContract.imports.map((item) => item.contract));
  const writeLog = globalThis.__writeLog;
  delete globalThis.__writeLog;
  globalThis.console = Object.freeze(
    Object.fromEntries(
      ["log", "debug", "info", "warn", "error"].map((level) => [
        level,
        (...args) => {
          writeLog(
            level,
            args.map((value) => (typeof value === "string" ? value : String(value))).join(" "),
          );
        },
      ]),
    ),
  );

  /** Encode a portable operation for the current host-issued scope. */
  function sync(operation, input = {}) {
    return JSON.parse(__hostSync(JSON.stringify({ operation, ...input })));
  }

  /** Await a host operation without accepting guest-supplied context identities. */
  async function call(operation, input = {}) {
    return JSON.parse(await __hostAsync(JSON.stringify({ operation, ...input })));
  }

  /** Retain a guest callback without transferring executable code to the host. */
  function retain(callback) {
    const id = next++;
    callbacks.set(id, callback);
    owners.set(id, declaring);
    return id;
  }

  /** Create a declaration proxy whose operations are checked by the outer host. */
  function declaration(owner) {
    /** Avoid retaining callbacks after the factory's declaration pass has ended. */
    function checkFactory() {
      if (declaring !== owner) throw new Error("Declaration factory has ended");
    }
    /** Bind every issued proxy to its actual environment identity. */
    const scoped = (operation, input = {}) => sync(operation, { scope: owner, ...input });
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
      state(initial, options = {}) {
        checkFactory();
        if (options.migrate) throw new Error("State migration is not implemented by this adapter");
        /** Reject asynchronous updates and boolean validator refusals before adoption. */
        const validate = (value) => {
          if (value?.then) throw new Error("State must be synchronous");
          value = stateValues.protect(value);
          const result = options.validate?.(value);
          if (result !== undefined && result !== true) throw new Error("State validation failed");
          return value;
        };
        initial = validate(initial);
        const check = options.validate
          ? retain((value) => {
              validate(value);
              return null;
            })
          : null;
        const id = scoped("state.declare", {
          initial,
          version: options.version ?? 1,
          validate: check,
        });
        const view = stateValues.readable(() => scoped("state.read", { id }));
        if (check !== null) validate(view.value);
        return Object.freeze({
          get value() {
            return view.value;
          },
          select(compute) {
            return view.select(compute);
          },
          subscribe() {
            return view.subscribe();
          },
          update(context, change) {
            context.check();
            const value = validate(change(view.value));
            return view.adopt(scoped("state.update", { id, value }));
          },
        });
      },
      get(service, key = ["main"]) {
        checkFactory();
        const id = scoped("reference.declare", { service, key });
        const state = stateValues.readable(() => scoped("reference.state", { id }));
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
            return (...args) => {
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
      resource(name, options) {
        checkFactory();
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
        const close = retain(async (id) => {
          const value = values.get(id);
          await options.close?.(value, context());
          values.delete(id);
          owners.delete(id);
        });
        const id = scoped("resource.declare", {
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
    if (Object.keys(orb.hooks ?? {}).length || Object.keys(orb.contributes ?? {}).length)
      throw new Error("Hook execution is not implemented by this adapter");
    return JSON.stringify({
      name: orb.name,
      apiVersion: orb.apiVersion,
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
      const result = callbacks.get(request.callback)(request.value);
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
      const request = JSON.parse(packet);
      const orb = globalThis.__tembraOrb;
      let result;
      switch (request.kind) {
        case "create": {
          result = JSON.parse(globalThis.__tembraDeclare(packet));
          break;
        }
        case "invoke":
          result = await orb.services[request.service].methods[request.method](
            environments.get(request.environment),
            context(),
            ...wire.arguments(request.service, request.method, request.args),
          );
          result = wire.result(request.service, request.method, result);
          break;
        case "callback":
          result = await callbacks.get(request.callback)(request.value);
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
      if (activeCallback === callback) activeCallback = null;
    }
  };

  globalThis.__tembraAbandon = () => {
    activeCallback = null;
  };

  globalThis.tembra = Object.freeze({
    wait(milliseconds) {
      return call("wait", { milliseconds });
    },
    ...(imports.has("wasi:filesystem/preopens@0.3.0")
      ? {
          filesystem: Object.freeze({
            async readText(path) {
              return readText(path);
            },
            async writeText(path, value) {
              writeText(path, value);
            },
          }),
        }
      : {}),
    ...(imports.has("wasi:sockets/types@0.3.0")
      ? {
          transport: Object.freeze({
            async exchange(ip, port, request) {
              return exchange(ip, port, request);
            },
          }),
        }
      : {}),
  });
})();
