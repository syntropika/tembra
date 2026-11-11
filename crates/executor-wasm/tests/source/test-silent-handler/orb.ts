import { orb } from "@syntropika/tembra-sdk";
import type { Response } from "wasi:http/types@0.3.0";

// User code whose handler settles without a response: neither a `response` nor an `error-code`.
export default orb({
  name: "web",
  apiVersion: 1,
  services: {
    handler: {
      methods: {
        async handle(): Promise<Response> {
          return undefined as unknown as Response;
        },
      },
    },
  },
});
