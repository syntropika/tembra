// State crosses heaps as bounded JSON data; unsupported values fail before adoption.
(() => {
  /** Copy and freeze data without silently changing dates, functions, integers or cycles. */
  function protect(value: unknown, ancestors = new Set<object>(), depth = 0): Json {
    if (depth > 64) throw new Error("State nesting exceeds the supported profile");
    if (value === null || typeof value === "string" || typeof value === "boolean") return value;
    if (
      typeof value === "number" &&
      Number.isFinite(value) &&
      !Object.is(value, -0) &&
      (!Number.isInteger(value) || Number.isSafeInteger(value))
    )
      return value;
    if (typeof value !== "object" || ancestors.has(value))
      throw new Error("State requires finite JSON data without cycles");
    if (
      !Array.isArray(value) &&
      Object.getPrototypeOf(value) !== Object.prototype &&
      Object.getPrototypeOf(value) !== null
    )
      throw new Error("State objects require a JSON data codec");
    if (value === null) return null;
    ancestors.add(value);
    const copy = Array.isArray(value)
      ? Array.from(value, (item) => protect(item, ancestors, depth + 1))
      : Object.fromEntries(
          Object.entries(value).map(([name, item]) => [name, protect(item, ancestors, depth + 1)]),
        );
    ancestors.delete(value);
    return Object.freeze(copy);
  }

  /** Derive values only when the retained snapshot changes. */
  function selection(
    source: { readonly value: Json },
    compute: (value: Json) => Json,
  ): GuestReadable {
    let previous: Json | symbol = Symbol();
    let previousResult: Json | symbol = Symbol();
    let selected: Json = null;
    const view = Object.freeze({
      get value() {
        const value = source.value;
        if (!Object.is(previous, value)) {
          const next = compute(value);
          if (!Object.is(previousResult, next)) selected = protect(next);
          previous = value;
          previousResult = next;
        }
        return selected;
      },
      select(next: (value: Json) => Json) {
        return selection(view, next);
      },
      subscribe() {
        throw new Error("Observable state subscriptions are not implemented by this profile");
      },
    });
    return view;
  }

  /**
   * Retain snapshot identity until its kernel revision changes. Each read names the retained
   * revision so the host may omit an unchanged value; a changed revision must carry its value.
   */
  function readable(read: (known: string | undefined) => Snapshot) {
    let revision: string | undefined;
    let value: Json = null;
    const binding = {
      adopt(snapshot: Snapshot) {
        if (revision !== snapshot.revision) {
          if (!Object.hasOwn(snapshot, "value"))
            throw new Error("Host omitted a changed state value");
          value = protect(snapshot.value);
          revision = snapshot.revision;
        }
        return value;
      },
      get value() {
        return binding.adopt(read(revision));
      },
      select(compute: (value: Json) => Json) {
        return selection(binding, compute);
      },
      subscribe() {
        throw new Error("Observable state subscriptions are not implemented by this profile");
      },
    };
    return Object.freeze(binding);
  }

  globalThis.__tembraState = Object.freeze({ protect, readable });
})();
