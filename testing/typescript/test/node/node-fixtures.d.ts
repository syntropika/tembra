import type {
  portableHooks,
  javascriptOpaque,
  hookBoundaries,
  hookPhysicalSettlement,
} from "@tembra/testing/conformance";

declare global {
  var __tembraCompiledLoaderTest:
    | {
        capability: object;
        closed: number;
        wait: () => Promise<void>;
      }
    | undefined;
  var __tembraSourceTest:
    | {
        capability: object;
        closed: number;
        wait: () => Promise<void>;
        observe?: (read: () => number) => void;
      }
    | undefined;
  var __tembraSourceHelpers:
    | {
        portableHooks: typeof portableHooks;
        javascriptOpaque: typeof javascriptOpaque;
        hookBoundaries: typeof hookBoundaries;
        hookPhysicalSettlement: typeof hookPhysicalSettlement;
      }
    | undefined;
  var __tembraLoaderGate: { enter: () => void; gate: Promise<void> } | undefined;
  var __tembraLoaderStartup: { enter: () => void; closed: number } | undefined;
  var __tembraLoaderExecuted: boolean | undefined;
  var __tembraLoaderOverBudget: boolean | undefined;
  var __tembraLoaderBatch: number | undefined;
  var __tembraSourceProfile: unknown;
  var __tembraSmall: boolean | undefined;
  var __tembraOversize: boolean | undefined;
}
