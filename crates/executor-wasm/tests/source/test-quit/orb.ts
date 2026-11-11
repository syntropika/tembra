import { orb } from "@syntropika/tembra-sdk";
import { exitWithCode } from "wasi:cli/exit@0.3.0";

export default orb({
  name: "quit",
  apiVersion: 1,
  services: {
    run: {
      methods: {
        async go() {
          // Exit is not an exception: guest catch blocks cannot intercept or survive it.
          try {
            exitWithCode(4);
          } catch (error) {
            return `caught: ${String(error)}`;
          }
          return "unreachable";
        },
      },
    },
  },
});
