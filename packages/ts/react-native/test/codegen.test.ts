import { expect, test } from "bun:test";
import { createRequire } from "node:module";
import { resolve } from "node:path";

interface Annotation {
  type: string;
  elementType?: Annotation;
}

interface Schema {
  modules: Record<
    string,
    {
      spec: {
        methods: {
          name: string;
          typeAnnotation: {
            params: { typeAnnotation: Annotation }[];
            returnTypeAnnotation: Annotation;
          };
        }[];
      };
    }
  >;
}

/** Validate the actual Codegen contract without emitting files or requiring a native SDK. */
test("Codegen accepts the binary carrier with a synchronous buffer take", () => {
  const require = createRequire(import.meta.url);
  const codegen = createRequire(require.resolve("react-native/package.json"));
  const { TypeScriptParser } = codegen(
    "@react-native/codegen/lib/parsers/typescript/parser.js",
  ) as { TypeScriptParser: new () => { parseFile(path: string): Schema } };
  const schema = new TypeScriptParser().parseFile(
    resolve(import.meta.dir, "../src/NativeTembra.ts"),
  );
  const methods = new Map(
    schema.modules["NativeTembra"]!.spec.methods.map((method) => [
      method.name,
      method.typeAnnotation,
    ]),
  );
  const annotation = (value: Annotation): string =>
    value.elementType ? `${value.type}<${annotation(value.elementType)}>` : value.type;
  const signature = (name: string) => {
    const method = methods.get(name)!;
    return {
      parameters: method.params.map((parameter) => annotation(parameter.typeAnnotation)),
      result: annotation(method.returnTypeAnnotation),
    };
  };
  const strings = (count: number) => Array<string>(count).fill("StringTypeAnnotation");
  expect(signature("portBeginIngress")).toEqual({
    parameters: [...strings(4), "ArrayBufferTypeAnnotation"],
    result: "BooleanTypeAnnotation",
  });
  expect(signature("portSupplyRequest")).toEqual({
    parameters: [...strings(3), "ArrayBufferTypeAnnotation"],
    result: "BooleanTypeAnnotation",
  });
  expect(signature("portTakeIngress")).toEqual({
    parameters: strings(2),
    result: "ArrayBufferTypeAnnotation",
  });
  expect(signature("portReceiveIngress")).toEqual({
    parameters: strings(2),
    result: "PromiseTypeAnnotation<StringTypeAnnotation>",
  });
  // Source Orbs are components that carry their own interpreter: no engine bytes cross here.
  expect(signature("configureExecution")).toEqual({
    parameters: strings(2),
    result: "PromiseTypeAnnotation<VoidTypeAnnotation>",
  });
  for (const name of ["portPullResponse", "portDisconnect"])
    expect(signature(name)).toEqual({
      parameters: strings(2),
      result: "BooleanTypeAnnotation",
    });
});
