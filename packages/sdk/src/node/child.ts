import { isManagedMessage } from "../manager/channel.js";
import { serveManagedHost } from "../manager/child.js";
import type { ManagedChannel } from "../manager/index.js";
import { createHost } from "./index.js";

const configuration: unknown = JSON.parse(process.argv[2] ?? "null");
if (
  configuration === null ||
  typeof configuration !== "object" ||
  !("module" in configuration) ||
  typeof configuration.module !== "string"
) {
  throw new Error("invalid child bootstrap configuration");
}
const { module } = configuration;
const binary =
  "binary" in configuration && typeof configuration.binary === "string"
    ? configuration.binary
    : undefined;
const channel: ManagedChannel = {
  send(message) {
    if (!process.send || !process.connected) {
      throw new Error("parent IPC disconnected");
    }
    process.send(message);
  },
  onMessage(listener) {
    const handler = (value: unknown) => {
      if (isManagedMessage(value)) {
        listener(value);
      }
    };
    process.on("message", handler);
    return () => {
      process.off("message", handler);
    };
  },
  onFailure(listener) {
    const handler = () => {
      listener(new Error("parent disconnected"));
    };
    process.on("disconnect", handler);
    return () => {
      process.off("disconnect", handler);
    };
  },
  // oxlint-disable-next-line typescript/require-await -- Uniform driver disposal is asynchronous; process IPC detach is synchronous.
  async terminate() {
    if (process.connected) {
      process.disconnect?.();
    }
    process.exit(1);
  },
};
serveManagedHost(
  channel,
  async (name) => await createHost({ name, ...(binary === undefined ? {} : { binary }) }),
  async (host) => {
    await host.load(module);
  },
);
