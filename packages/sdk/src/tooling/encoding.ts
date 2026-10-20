import ts from "typescript";

type EncodingResult = {
  read: number;
  written: number;
};
function width(point: number): number {
  if (point < 128) {
    return 1;
  }
  if (point < 2048) {
    return 2;
  }
  return point < 65_536 ? 3 : 4;
}
function encodeInto(source: string, destination: Uint8Array): EncodingResult {
  let read = 0;
  let written = 0;
  while (read < source.length) {
    const scalar = source.codePointAt(read);
    if (scalar === undefined) {
      break;
    }
    const consumed = scalar > 65_535 ? 2 : 1;
    const point = scalar >= 55_296 && scalar <= 57_343 ? 65_533 : scalar;
    const length = width(point);
    if (written + length > destination.length) {
      break;
    }
    if (length === 1) {
      destination[written] = point;
    } else if (length === 2) {
      destination[written] = 192 + Math.floor(point / 64);
    } else if (length === 3) {
      destination[written] = 224 + Math.floor(point / 4096);
    } else {
      destination[written] = 240 + Math.floor(point / 262_144);
      destination[written + 1] = 128 + (Math.floor(point / 4096) % 64);
    }
    if (length >= 3) {
      destination[written + length - 2] = 128 + (Math.floor(point / 64) % 64);
    }
    if (length >= 2) {
      destination[written + length - 1] = 128 + (point % 64);
    }
    written += length;
    read += consumed;
  }
  return { read, written };
}
/** WHATWG UTF-8 encoder bundled lexically for the server's ECMAScript-only executor. */
export class TextEncoder {
  readonly encoding = "utf-8";
  readonly encodeInto = encodeInto;

  encode(input = ""): Uint8Array {
    const output = new Uint8Array(input.length * 3);
    const { written } = this.encodeInto(input, output);
    return output.slice(0, written);
  }
}

/** Lexical server encoding support; no host global mutation is required. */
export function serverEncodingPrelude(): string {
  return `const TextEncoder = (() => {\n${width.toString()}\n${encodeInto.toString()}\nreturn ${TextEncoder.toString()};\n})();`;
}

/** Add the lexical encoder only when a bundle actually references the unprovided global. */
export function requiresServerEncoding(source: string): boolean {
  const filename = "/tembra-encoding.js";
  const options: ts.CompilerOptions = { allowJs: true, noLib: true, noResolve: true };
  const file = ts.createSourceFile(
    filename,
    source,
    ts.ScriptTarget.Latest,
    true,
    ts.ScriptKind.JS,
  );
  const host = ts.createCompilerHost(options);
  host.getSourceFile = (name) => (name === filename ? file : undefined);
  host.fileExists = (name) => name === filename;
  host.readFile = (name) => (name === filename ? source : undefined);
  const checker = ts.createProgram([filename], options, host).getTypeChecker();
  const visit = (node: ts.Node): boolean => {
    if (ts.isIdentifier(node) && node.text === "TextEncoder") {
      const { parent } = node;
      const property =
        (ts.isPropertyAccessExpression(parent) && parent.name === node) ||
        ((ts.isPropertyAssignment(parent) ||
          ts.isMethodDeclaration(parent) ||
          ts.isPropertyDeclaration(parent) ||
          ts.isGetAccessorDeclaration(parent) ||
          ts.isSetAccessorDeclaration(parent)) &&
          parent.name === node) ||
        (ts.isBindingElement(parent) && parent.propertyName === node) ||
        ts.isLabeledStatement(parent) ||
        ts.isBreakStatement(parent) ||
        ts.isContinueStatement(parent);
      const symbol = ts.isShorthandPropertyAssignment(parent)
        ? checker.getShorthandAssignmentValueSymbol(parent)
        : checker.getSymbolAtLocation(node);
      if (!property && symbol === undefined) {
        return true;
      }
    }
    return ts.forEachChild(node, visit) === true;
  };
  return visit(file);
}
