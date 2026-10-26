import ts from "typescript";

/** Check orb source types before bundling an artifact. */
export function typecheck(entry: string, root: string): void {
  const configFile = ts.findConfigFile(root, ts.sys.fileExists);
  const loaded = configFile ? ts.readConfigFile(configFile, ts.sys.readFile) : { config: {} };
  if (loaded.error)
    throw new Error(ts.flattenDiagnosticMessageText(loaded.error.messageText, "\n"));
  const parsed = ts.parseJsonConfigFileContent(loaded.config, ts.sys, root);
  const program = ts.createProgram([entry], {
    ...parsed.options,
    target: ts.ScriptTarget.ES2023,
    module: ts.ModuleKind.ESNext,
    moduleResolution: ts.ModuleResolutionKind.Bundler,
    allowImportingTsExtensions: true,
    noEmit: true,
    strict: true,
    skipLibCheck: true,
  });
  const diagnostics = ts.getPreEmitDiagnostics(program);
  if (diagnostics.length)
    throw new Error(
      ts.formatDiagnosticsWithColorAndContext(diagnostics, {
        /** Resolve diagnostics relative to the configured project root. */
        getCurrentDirectory: () => root,
        /** Keep file names unchanged when formatting diagnostics. */
        getCanonicalFileName: (name) => name,
        /** Use portable line endings in build diagnostics. */
        getNewLine: () => "\n",
      }),
    );
}
