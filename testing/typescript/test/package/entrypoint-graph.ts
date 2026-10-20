// Isolate Bun compiler file readers from globals and plugins exercised by other SDK tests.
import { fileURLToPath } from "node:url";

const [request, target] = process.argv.slice(2);
if (request === undefined || (target !== "node" && target !== "browser")) {
  throw new TypeError("Expected an SDK subpath and node or browser target");
}
const inputs: string[] = [];
const result = await Bun.build({
  entrypoints: [fileURLToPath(import.meta.resolve(request))],
  target,
  format: "esm",
  plugins: [
    {
      name: "inspect-sdk-entrypoint-graph",
      setup(builder) {
        builder.onLoad({ filter: /.*/u }, (args) => {
          inputs.push(args.path.replaceAll("\\", "/"));
          return undefined;
        });
      },
    },
  ],
});
if (!result.success) {
  throw new AggregateError(result.logs, `Cannot bundle ${request}`);
}
process.stdout.write(JSON.stringify(inputs));
