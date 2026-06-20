/**
 * Trivial demo of `useImperativeViz`: it appends a single <svg> child to its container
 * imperatively (mimicking d3 mounting an SVG) and removes it on cleanup. The StrictMode
 * test mounts/unmounts/remounts this and asserts exactly ONE <svg> survives (no leak,
 * no duplicate) — the contract every D10-D12 viz must satisfy.
 */
import { useRef } from 'react';
import { useImperativeViz } from './useImperativeViz';
import { demoVizCounters } from './demoVizState';

export function DemoViz() {
  const ref = useRef<HTMLDivElement>(null);

  useImperativeViz(ref, (el) => {
    demoVizCounters.setups += 1;
    const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
    svg.setAttribute('data-demo-viz', 'true');
    el.appendChild(svg);
    return () => {
      demoVizCounters.cleanups += 1;
      svg.remove();
    };
  }, []);

  return <div ref={ref} data-testid="demo-viz-container" />;
}
