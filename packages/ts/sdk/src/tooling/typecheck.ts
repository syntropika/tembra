import ts from "typescript";
import path from "node:path";
import { fileURLToPath } from "node:url";

/** Check orb source types before bundling an artifact. */
export function typecheck(
  entry: string,
  root: string,
  virtual: ReadonlyMap<string, string> = new Map(),
  conditions: readonly string[] = [],
): ts.Program {
  const configFile = ts.findConfigFile(root, ts.sys.fileExists);
  const loaded = configFile ? ts.readConfigFile(configFile, ts.sys.readFile) : { config: {} };
  if (loaded.error)
    throw new Error(ts.flattenDiagnosticMessageText(loaded.error.messageText, "\n"));
  const parsed = ts.parseJsonConfigFileContent(
    loaded.config,
    ts.sys,
    configFile ? path.dirname(configFile) : root,
  );
  const options: ts.CompilerOptions = {
    ...parsed.options,
    target: ts.ScriptTarget.ES2023,
    module: ts.ModuleKind.ESNext,
    moduleResolution: ts.ModuleResolutionKind.Bundler,
    allowImportingTsExtensions: true,
    noEmit: true,
    strict: true,
    skipLibCheck: true,
    lib: ["lib.es2023.d.ts"],
    types: [],
    allowArbitraryExtensions: true,
    customConditions: [...conditions, "source"],
  };
  const host = ts.createCompilerHost(options);
  const readFile = host.readFile;
  const fileExists = host.fileExists;
  const getSourceFile = host.getSourceFile;
  const directoryExists = host.directoryExists;
  const directories = new Set<string>();
  for (const file of virtual.keys()) {
    let directory = path.dirname(file);
    while (!directories.has(directory)) {
      directories.add(directory);
      const parent = path.dirname(directory);
      if (parent === directory) break;
      directory = parent;
    }
  }
  host.directoryExists = (directory) =>
    directories.has(directory) || directoryExists?.(directory) === true;
  host.readFile = (file) => virtual.get(file) ?? readFile(file);
  host.fileExists = (file) => virtual.has(file) || fileExists(file);
  host.getSourceFile = (file, language, onError, create) => {
    const source = virtual.get(file);
    return source === undefined
      ? getSourceFile(file, language, onError, create)
      : ts.createSourceFile(
          file,
          source,
          typeof language === "object" ? language.languageVersion : language,
          true,
        );
  };
  // The package entry locates its canonical guest declarations in source or installed output.
  const packageEntry = new URL(import.meta.resolve("@syntropika/tembra-sdk"));
  const environment = fileURLToPath(
    new URL(packageEntry.pathname.endsWith(".ts") ? "guest.ts" : "guest.d.ts", packageEntry),
  );
  // Include selected ambient declarations without typechecking unrelated build entrypoints.
  const ambient = parsed.fileNames.filter((file) => /\.d\.[cm]?ts$/.test(file));
  const program = ts.createProgram([entry, environment, ...ambient], options, host);
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
  return program;
}
