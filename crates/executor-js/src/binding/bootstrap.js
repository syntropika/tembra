(() => {
  const createObject = Object.create;
  const freezeObject = Object.freeze;
  const jsonStringify = JSON.stringify;
  const applyFunction = Reflect.apply;
  const { ownKeys } = Reflect;
  const prototypeOf = Object.getPrototypeOf;
  const ownDescriptor = Object.getOwnPropertyDescriptor;
  const { hasOwn } = Object;
  const { prototype: objectPrototype } = Object;
  const { isArray } = Array;
  const weakGet = Function.prototype.call.bind(WeakMap.prototype.get);
  const weakSet = Function.prototype.call.bind(WeakMap.prototype.set);
  const setHas = Function.prototype.call.bind(Set.prototype.has);
  const setAdd = Function.prototype.call.bind(Set.prototype.add);
  const setDelete = Function.prototype.call.bind(Set.prototype.delete);
  const recordHas = Function.prototype.call.bind(WeakSet.prototype.has);
  const recordAdd = Function.prototype.call.bind(WeakSet.prototype.add);
  const SetType = Set;
  const { prototype: arrayPrototype } = Array;
  const TypeErrorType = TypeError;
  const finiteNumber = Number.isFinite;
  const integralNumber = Number.isInteger;
  const safeInteger = Number.isSafeInteger;
  const charCode = Function.prototype.call.bind(String.prototype.charCodeAt);
  const ErrorType = Error;
  const defineOwn = Object.defineProperty;
  const symbolFor = Symbol.for;
  const nativeThenable = () => {
    const error = new ErrorType(
      "native output must not be a thenable or have an excessive prototype chain",
    );
    defineOwn(error, "tembraRuntimeError", {
      value:
        '{"code":"invalid","reason":"native_thenable","message":"native output must not be a thenable or have an excessive prototype chain","details":null}',
    });
    defineOwn(error, symbolFor("errorAdditionalPropertyKeys"), { value: ["tembraRuntimeError"] });
    return error;
  };
  const rejectedPromise = Promise.reject.bind(Promise);
  const raw = new WeakMap();
  const { ops } = Deno.core;
  const invoke = (cap, name, args = []) => {
    const result = ops.op_tembra(cap, name, args);
    if (result && result.__tembra_error) {
      const error = Object.assign(new Error(result.message), result);
      error.tembraRuntimeError = JSON.stringify({
        code: result.code,
        reason: result.reason,
        message: result.message,
        details: result.details,
      });
      error[Symbol.for("errorAdditionalPropertyKeys")] = ["tembraRuntimeError"];
      throw error;
    }
    return result;
  };
  const capability = (value) => {
    const cap = raw.get(value);
    if (!cap) {
      throw new TypeError("a backend-issued capability is required");
    }
    return cap;
  };
  // oxlint-disable-next-line unicorn/consistent-function-scoping -- bootstrap helpers remain private inside this initialization scope
  const normalize = (value) => {
    if (Array.isArray(value)) {
      return value.map(normalize);
    }
    if (value && typeof value === "object") {
      return Object.fromEntries(Object.keys(value).map((key) => [key, normalize(value[key])]));
    }
    return value;
  };
  const scalarString = (value) => {
    for (let index = 0; index < value.length; index += 1) {
      const point = charCode(value, index);
      if (point >= 0xd8_00 && point <= 0xdb_ff) {
        index += 1;
        const next = charCode(value, index);
        if (!(next >= 0xdc_00 && next <= 0xdf_ff)) {
          throw new TypeErrorType("unpaired high surrogate");
        }
      } else if (point >= 0xdc_00 && point <= 0xdf_ff) {
        throw new TypeErrorType("unpaired low surrogate");
      }
    }
  };
  const portable = (source, startingDepth = 0) => {
    const active = new SetType();
    // oxlint-disable-next-line complexity -- this boundary validates each JSON kind before copying through captured primordials
    const copy = (value, depth) => {
      if (depth > 128) {
        throw new TypeErrorType("portable depth limit");
      }
      if (value === null || typeof value === "boolean") {
        return value;
      }
      if (typeof value === "string") {
        scalarString(value);
        return value;
      }
      if (typeof value === "number") {
        if (!finiteNumber(value) || (integralNumber(value) && !safeInteger(value))) {
          throw new TypeErrorType(
            "number must be finite and integral values must be safe integers",
          );
        }
        return value;
      }
      if (typeof value !== "object") {
        throw new TypeErrorType("value is not strict JSON");
      }
      if (weakGet(raw, value)) {
        throw new TypeErrorType("issued capabilities are not portable");
      }
      if (depth >= 128) {
        throw new TypeErrorType("portable depth limit");
      }
      if (setHas(active, value)) {
        throw new TypeErrorType("cyclic portable value");
      }
      const array = isArray(value);
      const prototype = prototypeOf(value);
      if (
        array ? prototype !== arrayPrototype : prototype !== objectPrototype && prototype !== null
      ) {
        throw new TypeErrorType("native objects are not portable");
      }
      setAdd(active, value);
      let result;
      if (array) {
        const length = ownDescriptor(value, "length").value;
        if (ownKeys(value).length !== length + 1) {
          throw new TypeErrorType("array must be dense without custom properties");
        }
        result = [];
        for (let index = 0; index < length; index += 1) {
          const property = ownDescriptor(value, index);
          if (!property || !hasOwn(property, "value") || !property.enumerable) {
            throw new TypeErrorType("array element is missing or an accessor");
          }
          defineOwn(result, index, {
            value: copy(property.value, depth + 1),
            enumerable: true,
            writable: true,
            configurable: true,
          });
        }
      } else {
        result = createObject(objectPrototype);
        const keys = ownKeys(value);
        for (let index = 0; index < keys.length; index += 1) {
          const key = keys[index];
          if (typeof key !== "string") {
            throw new TypeErrorType("symbol property is not portable");
          }
          scalarString(key);
          const property = ownDescriptor(value, key);
          if (!property || !hasOwn(property, "value") || !property.enumerable) {
            throw new TypeErrorType("accessors and hidden properties are not portable");
          }
          defineOwn(result, key, {
            value: copy(property.value, depth + 1),
            enumerable: true,
            writable: true,
            configurable: true,
          });
        }
      }
      setDelete(active, value);
      return result;
    };
    return copy(source, startingDepth);
  };
  const draftValue = (draft) => {
    const property = Object.getOwnPropertyDescriptor(draft, "value");
    if (!property || !Object.hasOwn(property, "value") || !property.enumerable) {
      throw new TypeError("draft.value must be an enumerable data property");
    }
    return { value: portable(property.value) };
  };
  const reflectedGet = Reflect.get;
  const synchronous = (value) => {
    if (
      value !== null &&
      (typeof value === "object" || typeof value === "function") &&
      typeof reflectedGet(value, "then") === "function"
    ) {
      const data = {
        code: "invalid",
        reason: "async_declaration",
        message: "synchronous callback returned PromiseLike",
        details: null,
      };
      const error = Object.assign(new Error(data.message), data);
      error.tembraRuntimeError = JSON.stringify(data);
      error[Symbol.for("errorAdditionalPropertyKeys")] = ["tembraRuntimeError"];
      throw error;
    }
    return value;
  };
  const nativeRecords = new WeakSet();
  const nativeView = (source) => {
    const active = new SetType();
    const visit = (value, depth) => {
      if (depth > 128) {
        throw new TypeError("native record depth limit");
      }
      if (value === undefined) {
        return ["absent"];
      }
      const issued = weakGet(raw, value);
      if (issued && !recordHas(nativeRecords, value)) {
        return ["cap", issued];
      }
      try {
        return ["portable", portable(value, depth)];
      } catch (error) {
        if (!value || typeof value !== "object" || isArray(value)) {
          throw error;
        }
        const prototype = prototypeOf(value);
        if (prototype !== null && prototype !== objectPrototype) {
          throw error;
        }
        if (depth >= 128 || setHas(active, value)) {
          throw new TypeError("invalid native record nesting", { cause: error });
        }
        setAdd(active, value);
        const entries = [];
        const keys = ownKeys(value);
        for (let index = 0; index < keys.length; index += 1) {
          const key = keys[index];
          if (typeof key !== "string") {
            throw new TypeError("native record symbol field", { cause: error });
          }
          scalarString(key);
          const field = ownDescriptor(value, key);
          if (!field || !hasOwn(field, "value") || !field.enumerable) {
            throw new TypeError("native record must contain data fields", { cause: error });
          }
          defineOwn(entries, index, {
            value: [key, visit(field.value, depth + 1)],
            enumerable: true,
            writable: true,
            configurable: true,
          });
        }
        setDelete(active, value);
        return ["record", entries];
      }
    };
    return visit(source, 0);
  };
  const safeResolution = (value) => {
    if (value === null || (typeof value !== "object" && typeof value !== "function")) {
      return value;
    }
    let object = value;
    for (let depth = 0; object !== null && depth < 128; depth += 1, object = prototypeOf(object)) {
      const property = ownDescriptor(object, "then");
      if (!property) {
        continue;
      }
      if (!hasOwn(property, "value") || typeof property.value === "function") {
        throw nativeThenable();
      }
      return value;
    }
    if (object !== null) {
      throw nativeThenable();
    }
    return value;
  };
  const adapterFailure = (reason, message) => {
    const error = new ErrorType(message);
    const encoded = jsonStringify({ code: "invalid", reason, message, details: null });
    defineOwn(error, "tembraRuntimeError", { value: encoded });
    defineOwn(error, symbolFor("errorAdditionalPropertyKeys"), { value: ["tembraRuntimeError"] });
    return error;
  };
  const dataRecord = (value, label) => {
    if (
      value === null ||
      typeof value !== "object" ||
      isArray(value) ||
      (prototypeOf(value) !== objectPrototype && prototypeOf(value) !== null)
    ) {
      throw adapterFailure(
        label === "serve options" ? "serve_options" : "contract_adapter",
        `${label} must be a data record`,
      );
    }
    const result = createObject(null);
    const keys = ownKeys(value);
    for (let index = 0; index < keys.length; index += 1) {
      const key = keys[index];
      const property = ownDescriptor(value, key);
      if (
        typeof key !== "string" ||
        !property ||
        !hasOwn(property, "value") ||
        !property.enumerable
      ) {
        throw adapterFailure(
          label === "serve options" ? "serve_options" : "contract_adapter",
          `${label} requires enumerable data properties`,
        );
      }
      defineOwn(result, key, { value: property.value, enumerable: true, configurable: true });
    }
    return result;
  };
  const adapterEntries = (value) => {
    if (
      !isArray(value) ||
      prototypeOf(value) !== arrayPrototype ||
      ownKeys(value).length !== value.length + 1
    ) {
      throw adapterFailure("contract_adapter", "accepts requires a dense data array");
    }
    const result = [];
    for (let index = 0; index < value.length; index += 1) {
      const property = ownDescriptor(value, index);
      if (!property || !hasOwn(property, "value") || !property.enumerable) {
        throw adapterFailure("contract_adapter", "accepts requires data entries");
      }
      const entry = dataRecord(property.value, "adapter");
      if (
        ownKeys(entry).length !== 3 ||
        typeof entry.provider !== "string" ||
        typeof entry.consumer !== "string" ||
        typeof entry.adapt !== "function"
      ) {
        throw adapterFailure("contract_adapter", "adapter requires provider, consumer and adapt");
      }
      defineOwn(result, index, { value: freezeObject(entry), enumerable: true });
    }
    return freezeObject(result);
  };
  const serveOptions = (value) => {
    const flags = dataRecord(value, "serve options");
    const adapters = hasOwn(flags, "accepts") ? adapterEntries(flags.accepts) : [];
    delete flags.accepts;
    return { flags: portable(flags), adapters };
  };
  // oxlint-disable-next-line promise/prefer-await-to-callbacks -- callbacks preserve the synchronous event boundary and explicitly join rejection or physical completion
  const projectMethod = (callback) =>
    freezeObject((...args) => applyFunction(callback, undefined, args));
  const freezeProjection = (value) => {
    freezeObject(value.methods);
    freezeObject(value.state);
    freezeObject(value.opaque);
    return freezeObject(value);
  };
  const wrap = (kind, cap, metadata = {}) => {
    let value;
    const sync = (name, ...args) => invoke(cap, name, args);
    const cached = new Map();
    const once = (key, build) => {
      if (!cached.has(key)) {
        cached.set(key, build());
      }
      return cached.get(key);
    };
    if (kind === "scope") {
      value = {
        hostName: metadata.hostName,
        get signal() {
          return once("signal", () => sync("scope_signal"));
        },
        // oxlint-disable-next-line promise/prefer-await-to-callbacks -- callbacks preserve the synchronous event boundary and explicitly join rejection or physical completion
        root: (callback) => sync("root", callback),
        runHook: async (definition, context, input, options = {}) => {
          const result = await sync(
            "run_hook",
            portable(definition.descriptor),
            capability(context),
            portable(input === undefined ? { present: false } : { present: true, value: input }),
            options.terminal === undefined ? undefined : capability(options.terminal),
          );
          if (
            definition.descriptor.strategy === "parallel" &&
            definition.terminal === undefined &&
            definition.result === undefined
          ) {
            return result.map(() => undefined);
          }
          return result;
        },
        registry: (registry) => {
          const definition = portable(registry);
          return once(`registry:${JSON.stringify(definition)}`, () => sync("registry", definition));
        },
      };
    } else if (kind === "task_scope") {
      value = {
        get signal() {
          return once("signal", () => sync("task_signal"));
        },
        // oxlint-disable-next-line promise/prefer-await-to-callbacks -- callbacks preserve the synchronous event boundary and explicitly join rejection or physical completion
        root: (callback) => sync("root", callback),
      };
    } else if (kind === "event_sink") {
      value = (event) => sync("event_emit", event);
    } else if (kind === "setup") {
      value = {
        serve: (contract, factory, options = {}) => {
          const prepared = serveOptions(options);
          return sync(
            "serve",
            portable(contract.descriptor ?? contract),
            factory,
            prepared.flags,
            prepared.adapters,
          );
        },
      };
    } else if (kind === "declaration") {
      value = {
        principal: metadata.principal,
        cell: (name, definition) =>
          sync(
            "cell",
            name,
            portable(normalize(definition.schema)),
            definition.version ?? 1,
            Object.hasOwn(definition, "initial"),
            Object.hasOwn(definition, "initial") ? portable(definition.initial) : undefined,
            definition.initialize,
            definition.migrate,
          ),
        get: (contract, input, options = {}) =>
          sync(
            "get",
            portable(contract.descriptor ?? contract),
            portable(input),
            portable(options),
          ),
        resource: (name, definition) =>
          sync(
            "resource",
            name,
            definition.version ?? 1,
            definition.exclusive ?? false,
            (definition.deps ?? []).map(capability),
            definition.open,
            definition.close,
          ),
        derive: (sources, schema, compute) =>
          sync("derive", sources.map(capability), portable(normalize(schema)), compute),
        on: (name, source, handler, options = {}) =>
          sync("on", name, capability(source), handler, portable(options)),
        onEvent: (name, source, handler, options = {}) =>
          sync("on_event", name, source, handler, portable(options)),
        onResource: (name, resource, select, handler, options = {}) =>
          sync("on_resource", name, capability(resource), select, handler, portable(options)),
        task: (start) => sync("task", start),
        register: (registry, key, data) =>
          sync("register", portable(registry), key, portable(data)),
        registerMethod: (registry, key, data, method) =>
          sync("register_method", portable(registry), key, portable(data), method),
      };
    } else if (kind === "context") {
      value = {
        runtime: "tembra",
        get handle() {
          return value;
        },
        principal: metadata.principal,
        codeVersion: metadata.codeVersion,
        get signal() {
          return once("signal", () => sync("context_signal"));
        },
        check: () => sync("check"),
        cancel: (reason) => sync("cancel", reason ?? "cancelled"),
        get: (contract, input, options = {}) =>
          sync(
            "context_get",
            portable(contract.descriptor ?? contract),
            portable(input),
            portable(options),
          ),
      };
    } else if (kind === "signal") {
      value = {
        get aborted() {
          try {
            return sync("aborted");
          } catch (error) {
            if (error.reason === "expired") {
              return true;
            }
            throw error;
          }
        },
        get reason() {
          try {
            return sync("reason");
          } catch (error) {
            if (error.reason === "expired") {
              return error;
            }
            throw error;
          }
        },
        get cancelled() {
          return once("cancelled", () => {
            try {
              return sync("cancelled");
            } catch (error) {
              if (error.reason === "expired") {
                return Promise.resolve();
              }
              throw error;
            }
          });
        },
      };
    } else if (kind === "cell" || kind === "readable") {
      value = {
        get value() {
          return sync("snapshot").value;
        },
        snapshot: (context) => sync("snapshot", context ? capability(context) : undefined),
        // oxlint-disable-next-line promise/prefer-await-to-callbacks -- callbacks preserve the synchronous event boundary and explicitly join rejection or physical completion
        subscribe: (callback) => sync("subscribe", callback),
      };
      if (kind === "cell") {
        Object.assign(value, {
          get readable() {
            return value;
          },
          replace: (context, next, options = {}) =>
            sync("replace", capability(context), portable(next), portable(options)),
          change: (context, change) => sync("change", capability(context), change),
        });
      }
    } else if (kind === "resource") {
      value = {
        get value() {
          return sync("resource_value");
        },
        version: metadata.version,
        exclusive: metadata.exclusive,
      };
    } else if (kind === "unsubscribe") {
      let stopped = false;
      value = () => {
        if (!stopped) {
          stopped = true;
          sync("unsubscribe");
        }
      };
    } else if (kind === "reference") {
      const base = {
        contract: metadata.contract,
        get status() {
          return sync("status");
        },
        get shape() {
          return sync("shape");
        },
        get variant() {
          return sync("shape")?.variant;
        },
        get error() {
          return sync("error");
        },
        get ready() {
          return sync("ready");
        },
        release: () => sync("release"),
        // oxlint-disable-next-line promise/prefer-await-to-callbacks -- callbacks preserve the synchronous event boundary and explicitly join rejection or physical completion
        onStatus: (callback) => sync("on_status", callback),
        // oxlint-disable-next-line promise/prefer-await-to-callbacks -- callbacks preserve the synchronous event boundary and explicitly join rejection or physical completion
        onShape: (callback) => sync("on_shape", callback),
      };
      const cache = new Map();
      value = new Proxy(base, {
        get(target, key, receiver) {
          if (key === "then") {
            return undefined;
          }
          if (Reflect.has(target, key)) {
            return Reflect.get(target, key, receiver);
          }
          const descriptor = metadata.contract.descriptor ?? metadata.contract;
          const member =
            descriptor.variants[sync("shape")?.variant ?? ""]?.[key] ??
            Object.values(descriptor.variants)
              .map((m) => m[key])
              .find(Boolean);
          if (!member) {
            return undefined;
          }
          if (!cache.has(key)) {
            if (member.kind === "state") {
              cache.set(key, sync("readable", key));
            } else if (member.kind === "method") {
              cache.set(key, (...args) => {
                const present = Object.hasOwn(member, "args");
                const context = present ? args[1] : args[0];
                return sync(
                  "call",
                  key,
                  present,
                  present ? args[0] : undefined,
                  context ? capability(context) : undefined,
                );
              });
            } else {
              cache.set(key, sync("opaque", key));
            }
          }
          return cache.get(key);
        },
      });
    } else if (kind === "registry") {
      value = {
        table: (context) => sync("table", context ? capability(context) : undefined),
        // oxlint-disable-next-line promise/prefer-await-to-callbacks -- callbacks preserve the synchronous event boundary and explicitly join rejection or physical completion
        subscribe: (callback) => sync("registry_subscribe", callback),
      };
    } else if (kind === "operation") {
      value = { call: (context, args) => sync("operation", capability(context), args) };
    } else if (kind === "native") {
      value = Object.freeze(Object.create(null));
    } else if (kind === "callable") {
      value = (input) => {
        try {
          return sync("native_callable", input);
        } catch (error) {
          return rejectedPromise(error);
        }
      };
      Object.setPrototypeOf(value, null);
      Object.freeze(value);
    } else {
      throw new TypeError("unknown capability kind");
    }
    raw.set(value, cap);
    if (kind === "context") {
      void value.signal;
    }
    return value;
  };
  return {
    synchronous,
    wrap,
    draftValue,
    nativeView,
    safeResolution,
    adapterEntries,
    projectMethod,
    freezeProjection,
    emptyRecord: () => createObject(null),
    sealRecord: (cap, value) => {
      weakSet(raw, value, cap);
      recordAdd(nativeRecords, value);
      return freezeObject(value);
    },
    unwrap: capability,
    maybeRaw: (value) => weakGet(raw, value) ?? null,
    portable,
    normalize,
    table: (value) =>
      Object.freeze({ ...value, get: (key) => value.entries.find(([name]) => name === key)?.[1] }),
  };
})();
