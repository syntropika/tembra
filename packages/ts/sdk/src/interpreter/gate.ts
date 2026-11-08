/** Serialize one VM while allowing independent VMs and host callbacks to make progress. */
export class Gate {
  #busy = false;
  #queue: (() => void)[] = [];

  /** Ordinary contention waits for ownership; only factories need a bounded causal admission wait. */
  enter(milliseconds?: number, signal?: AbortSignal): Promise<() => void> {
    return new Promise((resolve, reject) => {
      let timer: ReturnType<typeof setTimeout> | undefined;
      const detach = () => {
        clearTimeout(timer);
        signal?.removeEventListener("abort", cancel);
      };
      const remove = (error: unknown) => {
        const index = this.#queue.indexOf(acquire);
        if (index < 0) return;
        this.#queue.splice(index, 1);
        detach();
        reject(error);
      };
      const cancel = () => remove(new Error("Operation cancelled before interpreter admission"));
      const acquire = () => {
        detach();
        this.#busy = true;
        let released = false;
        resolve(() => {
          if (released) return;
          released = true;
          const next = this.#queue.shift();
          if (next) next();
          else this.#busy = false;
        });
      };
      if (signal?.aborted) {
        reject(new Error("Operation cancelled before interpreter admission"));
        return;
      }
      if (!this.#busy) acquire();
      else {
        this.#queue.push(acquire);
        signal?.addEventListener("abort", cancel, { once: true });
        if (milliseconds !== undefined)
          timer = setTimeout(
            () =>
              remove(
                new Error(
                  "Interpreter factory admission timed out while another callback held the VM",
                ),
              ),
            milliseconds,
          );
      }
    });
  }
}
