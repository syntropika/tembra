import { workerModuleAcceptance } from "./browser-modules.js";

async function run(): Promise<void> {
  try {
    await workerModuleAcceptance();
    postMessage("passed");
  } catch (error) {
    postMessage(String(error));
  }
}
globalThis.addEventListener(
  "message",
  () => {
    // oxlint-disable-next-line promise/prefer-await-to-callbacks -- Event dispatch does not observe returned Promises; attach a rejection observer.
    run().catch((error: unknown) => {
      postMessage(String(error));
    });
  },
  { once: true },
);
