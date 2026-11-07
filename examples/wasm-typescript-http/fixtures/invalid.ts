import type { Report } from "example:readings/analysis";
const wrong: Report = {
  station: "greenhouse",
  count: 4,
  mean: "not a number",
  median: 20.5,
  minimum: 18,
  maximum: 25,
  alert: false,
  calibration: 0,
};
export { wrong };
