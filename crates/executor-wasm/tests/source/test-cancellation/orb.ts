import { orb } from "@syntropika/tembra-sdk";

// Every observable step is a log marker, so tests never infer progress from time.
export default orb({
  name: "probe",
  apiVersion: 1,
  services: {
    run: {
      methods: {
        async slow() {
          console.log("slow:started");
          try {
            await tembra.wait(4000);
          } catch {
            console.log("slow:cancelled");
            return 0;
          }
          return 1;
        },
        read() {
          console.log("read:executed");
          return 7;
        },
      },
    },
  },
});
