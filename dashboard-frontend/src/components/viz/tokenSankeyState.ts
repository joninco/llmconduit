/**
 * Test-observable counters for the `TokenSankey` d3-sankey wrapper. Kept out of the .tsx so the
 * component file exports only the component (react-refresh) — the DemoViz/Sparkline split.
 *
 * d3-sankey is a LAYOUT (no animation loop), so the StrictMode contract here is "no leaked /
 * duplicated SVG": the test asserts `setups`/`cleanups` balance and exactly one <svg> survives.
 */
export const tokenSankeyCounters = { setups: 0, cleanups: 0 };

export function resetTokenSankeyCounters(): void {
  tokenSankeyCounters.setups = 0;
  tokenSankeyCounters.cleanups = 0;
}
