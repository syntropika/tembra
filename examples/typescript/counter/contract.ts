import { Type, local, method } from "@tembra/sdk";

export const Counter = local("example.host.counter", {
  version: 1,
  input: Type.Object({}),
  members: { count: Type.Integer(), increment: method(undefined, Type.Integer()) },
});
