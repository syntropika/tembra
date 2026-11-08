import type { Context } from "../runtime/context.ts";
import type { Completion } from "./types.ts";

/** Check managed method implementations against the official generated WIT namespace. */
export type WitMethods<Interface, Environment> = {
  [
    Method in keyof Interface as Interface[Method] extends (...args: any[]) => any ? Method : never
  ]: Interface[Method] extends (...args: infer Arguments) => infer Result
    ? (
        environment: Environment,
        context: Context,
        ...args: Arguments
      ) => Completion<Awaited<Result>>
    : never;
};
