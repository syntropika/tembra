import { orb } from "@syntropika/tembra-sdk";
export default orb({
  name: "ambient",
  apiVersion: 1,
  services: {
    policy: {
      methods: {
        inspect() {
          const refused = (read: () => unknown) => {
            try {
              read();
              return false;
            } catch {
              return true;
            }
          };
          return JSON.stringify({
            now: refused(() => Date.now()),
            date: refused(() => new Date()),
            call: refused(() => Date()),
            constructor: refused(() => new (new Date(0).constructor as DateConstructor)()),
            random: refused(() => Math.random()),
            pure: new Date(0).toISOString(),
          });
        },
      },
    },
  },
});
