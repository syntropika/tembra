import { Type, canonicalJson, hostNamed, method, remote } from "../src/index.js";

const integer = { minimum: -2_147_483_648, maximum: 2_147_483_647 };
export const Counter = remote("fixture.typed-counter", {
  version: 1,
  input: Type.Object({ name: Type.String() }, { title: "CounterInput" }),
  host: hostNamed("service"),
  variants: {
    default: {
      state: { count: Type.Integer({ ...integer, title: "int32" }) },
      methods: {
        increment: method(
          Type.Object({ by: Type.Integer(integer) }, { title: "Increment" }),
          Type.Integer({ ...integer, title: "int32" }),
        ),
        read: method(undefined, Type.Integer({ ...integer, title: "int32" })),
        touch: method(),
      },
    },
  },
});

process.stdout.write(`${canonicalJson(Counter.descriptor)}\n`);
