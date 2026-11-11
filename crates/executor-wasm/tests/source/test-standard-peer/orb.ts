import { orb } from "@syntropika/tembra-sdk";
import { getDirectories } from "wasi:filesystem/preopens@0.3.0";

export default orb({
  name: "standard-peer",
  apiVersion: 1,
  services: {
    checks: {
      methods: {
        async burst() {
          const directories = getDirectories();
          try {
            const [[directory]] = directories;
            const results = await Promise.all(Array.from({ length: 96 }, () => directory!.stat()));
            return results.length.toString();
          } finally {
            for (const [directory] of directories) directory[Symbol.dispose]();
          }
        },
      },
    },
  },
});
