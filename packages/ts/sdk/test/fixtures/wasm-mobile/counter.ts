import { orb, type Declaration } from "@syntropika/tembra-sdk";

/** Mobile transport fixture: factories, per-Store heaps and attributed logs. */
export default orb({
  name: "counter",
  apiVersion: 1,
  services: {
    counter: {
      create(self: Declaration) {
        // The factory reads its kernel key; the value is the host's, not a guest default.
        if (!Array.isArray(self.key)) throw new Error("the factory has no key");
        // A second synchronous host operation, so a host can abandon the factory mid-way.
        const adds = self.state({ adds: 0 });
        return { value: 0, key: self.key.join("/"), adds };
      },
      methods: {
        add(instance: { value: number }) {
          console.log("mobile");
          return ++instance.value;
        },
      },
    },
  },
});
