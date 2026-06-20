/**
 * Test-observable counters for the DemoViz wrapper. Kept out of the .tsx so the
 * component file exports ONLY the component (react-refresh). The StrictMode test
 * asserts setups/cleanups stay balanced (no leaked imperative instance).
 */
export const demoVizCounters = { setups: 0, cleanups: 0 };

export function resetDemoVizCounters(): void {
  demoVizCounters.setups = 0;
  demoVizCounters.cleanups = 0;
}
