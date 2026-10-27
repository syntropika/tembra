import type { Host } from "../src/host.ts";
import {
  orb,
  type Context,
  type Declaration,
  type Readable,
  type ReferenceBase,
  type ServiceDefinition,
  type ServiceReference,
  type State,
} from "../src/index.ts";

const createAccount = (self: Declaration) => ({ balance: self.state({ amount: 0 }) });

export const Accounts = {
  create: createAccount,
  methods: {
    zero: () => 42,
    current: (environment: ReturnType<typeof createAccount>) => environment.balance.value.amount,
    add(environment: ReturnType<typeof createAccount>, ctx: Context, amount: number) {
      return environment.balance.update(ctx, (value) => ({ amount: value.amount + amount })).amount;
    },
    async greet(_environment: ReturnType<typeof createAccount>, _ctx: Context, name: string) {
      return await Promise.resolve(`Hello, ${name}`);
    },
    optional(_environment: ReturnType<typeof createAccount>, _ctx: Context, prefix?: string) {
      return prefix ?? "default";
    },
    optionalContext(_environment: unknown, _ctx?: Context) {
      return 1;
    },
    optionalEnvironment(_environment?: unknown) {
      return 2;
    },
    sum(_environment: unknown, _ctx: Context, ...values: number[]) {
      return values.reduce((total, value) => total + value, 0);
    },
  },
} satisfies ServiceDefinition;

const DirectState = {
  create: (self: Declaration) => self.state({ enabled: true }),
  methods: { enabled: (state: State<{ enabled: boolean }>) => state.value.enabled },
} satisfies ServiceDefinition;

const HiddenState = {
  create(self: Declaration) {
    self.state({ hidden: 1 });
    return { label: "unrelated" };
  },
  methods: {},
} satisfies ServiceDefinition;

const NoState = { methods: { read: () => "stateless" } } satisfies ServiceDefinition;

const ReservedMethods = {
  methods: {
    then: () => "hidden",
    ready: () => "hidden",
    status: () => "hidden",
    state: () => "hidden",
    release: () => "hidden",
    opaque: () => "hidden",
    read: () => 1,
  },
} satisfies ServiceDefinition;

const OptionalMetadata = {
  create(self: Declaration): { count: State<number>; label?: string } {
    return { count: self.state(0) };
  },
  methods: {},
} satisfies ServiceDefinition;

type Equal<A, B> =
  (<T>() => T extends A ? 1 : 2) extends <T>() => T extends B ? 1 : 2 ? true : false;
type Assert<T extends true> = T;

export type MethodAssertions = [
  Assert<Equal<ReturnType<ServiceReference<typeof Accounts>["zero"]>, Promise<number>>>,
  Assert<Equal<ReturnType<ServiceReference<typeof Accounts>["current"]>, Promise<number>>>,
  Assert<Equal<ReturnType<ServiceReference<typeof Accounts>["add"]>, Promise<number>>>,
  Assert<Equal<ReturnType<ServiceReference<typeof Accounts>["greet"]>, Promise<string>>>,
  Assert<Equal<ServiceReference<typeof HiddenState>["state"], Readable<unknown>>>,
  Assert<Equal<ServiceReference<typeof NoState>["state"], Readable<unknown>>>,
  Assert<Equal<keyof ServiceReference<typeof ReservedMethods>, keyof ReferenceBase | "read">>,
  Assert<
    Equal<
      Awaited<ServiceReference<typeof ReservedMethods>>,
      ServiceReference<typeof ReservedMethods>
    >
  >,
  Assert<Equal<ServiceReference<typeof ReservedMethods>["release"], ReferenceBase["release"]>>,
];

/** Compile-only calls: importing this fixture never acquires or invokes a reference. */
export async function checkTypedServices(host: Host, self: Declaration, ctx: Context) {
  const definition = orb({
    name: "typed-fixture",
    apiVersion: 1,
    services: { accounts: Accounts, direct: DirectState, hidden: HiddenState, none: NoState },
  });
  const account = await host.get(Accounts, "main");
  const captured = await host.get(definition.services.accounts, ["team", "main"]);
  const held = host.hold(Accounts, "held", { principal: "user", methods: ["current"] });
  const dependency = self.get(Accounts, "dependency");
  const scoped = await ctx.get(Accounts, "root");
  const reference: ServiceReference<typeof Accounts> = account;
  const amount: number = account.state.value.amount;
  const direct = await host.get(DirectState, "main");
  const enabled: boolean = direct.state.value.enabled;
  const zero: Promise<number> = account.zero();
  const zeroWithContext: Promise<number> = account.zero(ctx);
  const current: Promise<number> = captured.current(ctx);
  const added: Promise<number> = dependency.add(2, ctx);
  const greeting: Promise<string> = scoped.greet("Ada");
  const greetingWithContext: Promise<string> = held.greet("Ada", ctx);
  const defaultPrefix: Promise<string> = account.optional();
  const prefix: Promise<string> = account.optional("custom", ctx);
  const optionalContext: Promise<number> = account.optionalContext();
  const optionalEnvironment: Promise<number> = account.optionalEnvironment(ctx);
  const sum: Promise<number> = account.sum(1, 2, 3, ctx);
  const emptySum: Promise<number> = account.sum();
  const metadata = await host.get(OptionalMetadata, "main");
  const metadataValue: number = metadata.state.value;

  // @ts-expect-error Business arguments retain their declared types.
  account.add("wrong");
  // @ts-expect-error Required business arguments cannot be omitted.
  account.add();
  // @ts-expect-error Implementation environments are removed from public calls.
  account.current({ balance: {} });
  // @ts-expect-error Contexts are optional trailing arguments.
  account.add(ctx, 2);
  // @ts-expect-error Methods absent from the service are not dynamic properties.
  account.missing();
  // @ts-expect-error The other service does not gain account methods.
  direct.add(1);
  // @ts-expect-error State inferred from a returned environment preserves field types.
  const wrongAmount: string = account.state.value.amount;
  // @ts-expect-error Directly returned state preserves field types.
  const wrongEnabled: number = direct.state.value.enabled;
  const hidden = await host.get(HiddenState, "main");
  // @ts-expect-error Unreturned state is unknown rather than any.
  const hiddenValue: number = hidden.state.value.hidden;
  // @ts-expect-error Captured references retain the original method argument types.
  captured.greet(7);
  // @ts-expect-error Calls return promises, even for synchronous implementations.
  const synchronous: number = account.zero();
  // @ts-expect-error Rest arguments preserve their declared element type.
  account.sum("wrong");
  // @ts-expect-error Context cannot precede variadic business arguments.
  account.sum(ctx, 1);
  // @ts-expect-error Context cannot occur between variadic business arguments.
  account.sum(1, ctx, 2);
  // @ts-expect-error A trailing Context option must not widen variadic number arguments.
  account.sum(1, undefined, 2);
  // @ts-expect-error A zero-argument implementation accepts only an optional Context.
  account.zero("wrong");
  // @ts-expect-error Optional injected Context is not a business argument.
  account.optionalContext("wrong");
  // @ts-expect-error Optional injected environment is not a business argument.
  account.optionalEnvironment("wrong");

  return {
    reference,
    amount,
    enabled,
    zero,
    zeroWithContext,
    current,
    added,
    greeting,
    greetingWithContext,
    defaultPrefix,
    prefix,
    optionalContext,
    optionalEnvironment,
    sum,
    emptySum,
    metadataValue,
    wrongAmount,
    wrongEnabled,
    hiddenValue,
    synchronous,
  };
}
