/** Retain asynchronous WASI work even when a guest drops its returned future after queuing bytes. */
export class BrowserIO {
  readonly #pending = new Set<Promise<unknown>>();
  readonly #wrapped = new WeakMap<object, object>();
  readonly #original = new WeakMap<object, object>();

  /** Track physical settlement without converting an already observed I/O error into a second error. */
  track<T>(operation: Promise<T>): Promise<T> {
    this.#pending.add(operation);
    operation.then(
      () => this.#pending.delete(operation),
      () => this.#pending.delete(operation),
    );
    return operation;
  }

  /** Return only after the operations admitted by this callback have physically completed. */
  async settled(): Promise<void> {
    while (this.#pending.size) await Promise.allSettled([...this.#pending]);
  }

  /** Wrap per-instance descriptors rather than modifying the shim's shared resource prototypes. */
  descriptor(value: object): object {
    const existing = this.#wrapped.get(value);
    if (existing) return existing;
    const wrapped = new Proxy(value, {
      get: (target, key) => {
        const method = Reflect.get(target, key, target);
        if (typeof method !== "function") return method;
        return (...args: unknown[]) => {
          const result = method.apply(
            target,
            args.map((arg) =>
              typeof arg === "object" && arg !== null ? (this.#original.get(arg) ?? arg) : arg,
            ),
          );
          if (key === "openAt")
            return this.track(
              Promise.resolve(result).then((descriptor) => this.descriptor(descriptor)),
            );
          if (result instanceof Promise) return this.track(result);
          if (key === "readViaStream" && Array.isArray(result) && result[1] instanceof Promise)
            this.track(result[1]);
          return result;
        };
      },
    });
    this.#wrapped.set(value, wrapped);
    this.#original.set(wrapped, value);
    return wrapped;
  }
}
