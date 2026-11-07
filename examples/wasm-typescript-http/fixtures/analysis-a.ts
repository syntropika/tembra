import { z } from "zod";
import { get, publish } from "example:readings/capabilities";
import type * as Analysis from "example:readings/analysis";

const readings = z.object({
  station: z.string().min(1),
  samples: z.array(z.number().finite()).min(1).max(1000),
});

// This is the only source edit between the two demonstrated versions.
const calibration = 0;

export const analysis = {
  run(context, input) {
    try {
      publish(context, { stage: "fetching", message: "Requesting sensor readings" });
      const response = get(context, input.path);
      if (response.status !== 200) throw new Error(`HTTP ${response.status}`);

      const data = readings.parse(JSON.parse(response.body));
      publish(context, {
        stage: "calculating",
        message: `Processing ${data.samples.length} readings`,
      });
      const sorted = data.samples.map((value) => value + calibration).sort((a, b) => a - b);
      const count = sorted.length;
      const mean = sorted.reduce((sum, value) => sum + value, 0) / count;
      const middle = Math.floor(count / 2);
      const median = count % 2 ? sorted[middle]! : (sorted[middle - 1]! + sorted[middle]!) / 2;
      const report = {
        station: data.station,
        count,
        mean,
        median,
        minimum: sorted[0]!,
        maximum: sorted[count - 1]!,
        alert: mean > input.threshold,
        calibration,
      };
      publish(context, { stage: "done", message: `Mean: ${mean}; alert: ${report.alert}` });
      return report;
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      // Jco represents a WIT result error as an exception with a payload.
      throw Object.assign(new Error(message), { payload: message });
    }
  },
} satisfies typeof Analysis;
