/**
 * Test-observable counters for the `Sparkline` uPlot wrapper. Kept out of the .tsx so the
 * component file exports ONLY the component (react-refresh) — the same split DemoViz uses.
 *
 * The StrictMode test asserts `creates`/`destroys` stay balanced (no leaked uPlot instance,
 * no duplicate <canvas>) the same way the d3 demo asserts setups/cleanups balance.
 */
export const sparklineCounters = { creates: 0, destroys: 0 };

export function resetSparklineCounters(): void {
  sparklineCounters.creates = 0;
  sparklineCounters.destroys = 0;
}
