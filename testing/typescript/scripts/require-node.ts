// Preload before any acceptance module or native addon can execute.
if ("bun" in process.versions || process.release.name !== "node") {
  throw new Error(
    "Node acceptance requires the Node.js runtime; remove --bun from the parent script runner",
  );
}
