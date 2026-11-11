import { orb } from "@syntropika/tembra-sdk";
import { fetch } from "@syntropika/tembra-sdk/http";

/** Fetch `url`, read to the first blank line or the end, then release the whole response. */
async function first(url: string): Promise<string> {
  const response = await fetch(url);
  let text = "";
  try {
    for await (const chunk of response.body) {
      text += String.fromCharCode(...chunk);
      // An event stream never ends by itself: stop after its first event.
      if (text.includes("\n\n")) break;
    }
  } finally {
    // Release the body and trailers now, so the host may close the connection.
    await response.close();
  }
  return `${response.status}|${response.headers.get("content-type") ?? ""}|${text}`;
}

export default orb({
  name: "web",
  apiVersion: 1,
  services: {
    web: {
      methods: {
        async first(_environment: unknown, _context: unknown, url: string) {
          return first(url);
        },
        async strict(_environment: unknown, _context: unknown, url: string) {
          return first(url);
        },
      },
    },
  },
});
