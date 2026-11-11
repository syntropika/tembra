import { orb, type Context } from "@syntropika/tembra-sdk";

let detached = "none";
let resume: (() => void) | undefined;
let dormant = "pending";

export default orb({
  name: "b1",
  apiVersion: 1,
  services: {
    probe: {
      methods: {
        async detach(_environment: unknown, context: Context, milliseconds: number) {
          void tembra.wait(milliseconds).then(
            () => {
              // The SDK refuses a Context once its method returned; either outcome proves where
              // the continuation ran.
              try {
                detached = `ran: ${context.principal}`;
              } catch (error) {
                detached = `ran: ${(error as Error).message}`;
              }
            },
            (error: unknown) => {
              detached = `revoked: ${(error as Error).message}`;
            },
          );
        },
        observed() {
          return detached;
        },
        async park() {
          void new Promise<void>((settle) => {
            resume = settle;
          }).then(() => {
            dormant = "resumed";
          });
        },
        async poke() {
          resume?.();
          await Promise.resolve();
          return dormant;
        },
      },
    },
  },
});
