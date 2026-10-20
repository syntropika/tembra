import { Type, defineFacet } from "@tembra/sdk";

import { Counter } from "./contract.ts";

export const counterFacet = defineFacet({
  name: "example.counter",
  uses: [],
  setup(_scope, declaration) {
    declaration.serve(Counter, (_input, self) => {
      const count = self.cell("count", { schema: Type.Integer(), initial: 0 });
      return {
        state: { count },
        methods: {
          increment(context) {
            return count.replace(context, count.value + 1).value;
          },
        },
      };
    });
  },
});
export default counterFacet;
