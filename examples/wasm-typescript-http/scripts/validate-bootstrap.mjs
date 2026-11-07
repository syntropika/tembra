// Derive callable paths from Jco's generated forwarding code, without parsing WIT again.
import { parse } from "acorn";
export function validateBootstrap(source, world) {
  const paths = [];
  function visit(node) {
    if (!node || typeof node !== "object") return;
    if (
      node.type === "CallExpression" &&
      node.callee.type === "Identifier" &&
      ["$lookup", "$method", "$link"].includes(node.callee.name) &&
      node.arguments[0]?.type === "ArrayExpression"
    ) {
      if (node.callee.name !== "$lookup")
        throw new Error("Resource export validation is not implemented in this experiment");
      const path = node.arguments[0].elements.map((element) => {
        if (element?.type !== "Literal" || typeof element.value !== "string")
          throw new Error("Unexpected Jco forwarding path");
        return element.value;
      });
      if (path.length < 1 || path.length > 2) throw new Error("Unsupported callable export shape");
      if (!paths.some((existing) => JSON.stringify(existing) === JSON.stringify(path)))
        paths.push(path);
    }
    for (const value of Object.values(node))
      if (Array.isArray(value)) value.forEach(visit);
      else if (value && typeof value === "object") visit(value);
  }
  visit(parse(source, { ecmaVersion: "latest", sourceType: "module" }));
  if (paths.length === 0 || world.exports.some((name) => !paths.some((path) => path[0] === name)))
    throw new Error("Not every Jco export has a callable contract");
  const entries = paths.map((path) => ({
    root: path[0],
    method: path[1] ?? null,
    label: path.join("."),
  }));
  const helper = `
// Experimental preparation guard for plain function/interface exports.
// Intrinsics are captured before any candidate script is evaluated.
const $contractCreate = Object.create;
const $contractDescriptor = Object.getOwnPropertyDescriptor;
const $contractDefine = Object.defineProperty;
const $contractHasOwn = Object.hasOwn;
const $contractPrototype = Object.getPrototypeOf;
const $contractApply = Reflect.apply;
const $asyncFunctionPrototype = Object.getPrototypeOf(async function() {});
const $generatorFunctionPrototype = Object.getPrototypeOf(function*() {});
const $asyncGeneratorFunctionPrototype = Object.getPrototypeOf(async function*() {});
const $requiredCallables = ${JSON.stringify(entries)};
function $contractReadOwn(object, name) {
  const descriptor = $contractDescriptor(object, name);
  if (descriptor === undefined) throw "contract: missing export namespace";
  if ($contractHasOwn(descriptor, "value")) return descriptor.value;
  if (typeof descriptor.get === "function") return $contractApply(descriptor.get, object, []);
  throw "contract: unreadable export namespace";
}
function $contractFunction(value, label) {
  if (typeof value !== "function") throw "contract: missing callable " + label;
  const prototype = $contractPrototype(value);
  if (prototype === $asyncFunctionPrototype || prototype === $generatorFunctionPrototype || prototype === $asyncGeneratorFunctionPrototype)
    throw "contract: synchronous callable required for " + label;
  return value;
}
function $validatedExports(exports) {
  const stable = $contractCreate(null);
  const roots = $contractCreate(null);
  for (let index = 0; index < $requiredCallables.length; index++) {
    const entry = $requiredCallables[index];
    if (entry.method === null) {
      const callable = $contractFunction($contractReadOwn(exports, entry.root), entry.label);
      $contractDefine(stable, entry.root, {__proto__: null, value: callable, enumerable: true});
      continue;
    }
    let original;
    if ($contractHasOwn(roots, entry.root)) original = roots[entry.root];
    else {
      original = $contractReadOwn(exports, entry.root);
      if (original === null || (typeof original !== "object" && typeof original !== "function"))
        throw "contract: invalid namespace " + entry.root;
      roots[entry.root] = original;
      $contractDefine(stable, entry.root, {__proto__: null, value: $contractCreate(null), enumerable: true});
    }
    const descriptor = $contractDescriptor(original, entry.method);
    if (descriptor === undefined || !$contractHasOwn(descriptor, "value"))
      throw "contract: data method required for " + entry.label;
    const callable = $contractFunction(descriptor.value, entry.label);
    $contractDefine(stable[entry.root], entry.method, {__proto__: null, value: callable, enumerable: true});
  }
  return stable;
}
`;
  let output = source;
  for (const [needle, replacement] of [
    ["let $script = null;", helper + "\nlet $script = null;"],
    [
      "    $script = { name, exports };",
      "    $script = { name, exports: $validatedExports(exports) };",
    ],
    [
      'new Function("$import", "$exports", "$exportAll", $rewrite(source))',
      `new Function("$import", "$exports", "$exportAll", ${JSON.stringify('"use strict";\n')} + $rewrite(source))`,
    ],
  ]) {
    if (output.split(needle).length !== 2) throw new Error("Pinned Jco bootstrap shape changed");
    output = output.replace(needle, replacement);
  }
  return { source: output, paths };
}
