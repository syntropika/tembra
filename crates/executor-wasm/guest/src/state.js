// State crosses heaps as bounded JSON data; unsupported values fail before adoption.
(() => {
  /** Copy and freeze data without silently changing dates, functions, integers or cycles. */
  function protect(value, ancestors = new Set(), depth = 0) {
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
  function selection(source, compute) {
    let previous = Symbol();
    let previousResult = Symbol();
    let selected;
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
      select(next) {
        return selection(view, next);
      },
      subscribe() {
        throw new Error("Observable state subscriptions are not implemented by this profile");
      },
    });
    return view;
  }

  /** Retain snapshot identity until its kernel revision changes. */
  function readable(read) {
    let revision;
    let value;
    const binding = {
      adopt(snapshot) {
        if (revision !== snapshot.revision) {
          value = protect(snapshot.value);
          revision = snapshot.revision;
        }
        return value;
      },
      get value() {
        return binding.adopt(read());
      },
      select(compute) {
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
