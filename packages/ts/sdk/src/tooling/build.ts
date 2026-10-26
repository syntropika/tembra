import { createHash } from "node:crypto";
import { mkdir, rename, writeFile } from "node:fs/promises";
import path from "node:path";
import ts from "typescript";

import { external, resolver } from "./resolver.ts";
import { typecheck } from "./typecheck.ts";
import { Platform, type Artifact, type BuildConfig } from "./types.ts";

/** Resolves packages at build time and emits exactly one content-addressed ESM file per target. */
export async function build(config: BuildConfig): Promise<readonly Artifact[]> {
  const root = path.resolve(config.root ?? process.cwd());
  const outDir = path.resolve(root, config.outDir ?? "dist/tembra");
  const artifacts: Artifact[] = [];
  for (const [name, target] of Object.entries(config.builds)) {
    if (!/^[\w-]+$/.test(name) || !Object.values(Platform).includes(target.platform))
      throw new Error("Invalid build name or platform");
    const entry = path.resolve(root, target.entry);
    if (target.typecheck !== false) typecheck(entry, root);
    const result = await Bun.build({
      entrypoints: [target.platform === Platform.Embedded ? "tembra:entry" : entry],
      root,
      format: target.platform === Platform.Embedded ? "iife" : "esm",
      target: target.platform === Platform.Node ? "node" : "browser",
      splitting: false,
      minify: target.minify ?? true,
      ...(target.define ? { define: { ...target.define } } : {}),
      plugins: [resolver(root, target)],
    });
    if (!result.success) throw new AggregateError(result.logs, `Build ${name} failed`);
    if (result.outputs.length !== 1 || result.outputs[0]?.kind !== "entry-point")
      throw new Error(
        `Build ${name} requires external assets; publish them explicitly instead of silently expanding the orb bundle`,
      );
    const output = result.outputs[0]!;
    const source = await output.text();
    const parsed = ts.createSourceFile(
      "bundle.js",
      source,
      ts.ScriptTarget.Latest,
      true,
      ts.ScriptKind.JS,
    );
    const hasDefault = parsed.statements.some(
      (statement) =>
        ts.isExportAssignment(statement) ||
        (ts.isExportDeclaration(statement) &&
          statement.exportClause &&
          ts.isNamedExports(statement.exportClause) &&
          statement.exportClause.elements.some((element) => element.name.text === "default")),
    );
    if (target.platform !== Platform.Embedded && !hasDefault)
      throw new Error(`Build ${name} must export a default orb`);
    /** Reject imports that the selected runtime cannot resolve. */
    function verify(node: ts.Node): void {
      let specifier: ts.Expression | undefined;
      if (ts.isImportDeclaration(node) || ts.isExportDeclaration(node))
        specifier = node.moduleSpecifier;
      if (ts.isCallExpression(node) && node.expression.kind === ts.SyntaxKind.ImportKeyword)
        specifier = node.arguments[0];
      if (specifier && (!ts.isStringLiteralLike(specifier) || !external(specifier.text, target)))
        throw new Error(`Build ${name} contains an unresolved import`);
      ts.forEachChild(node, verify);
    }
    verify(parsed);
    const bytes = Buffer.from(source);
    const hash = createHash("sha256").update(bytes).digest("hex");
    const directory = path.join(outDir, name);
    const destination = path.join(directory, `orb.${hash}.js`);
    await mkdir(directory, { recursive: true });
    const temporary = `${destination}.${process.pid}.tmp`;
    await writeFile(temporary, bytes);
    await rename(temporary, destination);
    artifacts.push({
      name,
      platform: target.platform,
      path: destination,
      sha256: hash,
      bytes: bytes.length,
    });
  }
  return artifacts;
}
