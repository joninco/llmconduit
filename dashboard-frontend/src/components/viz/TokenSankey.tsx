/**
 * TokenSankey (D12) — the cost story. A 3-column d3-sankey: client → gateway → (upstream, served-
 * model). Band height ∝ tokens over the rolling window (`sankeyModel`, default 30 s); each band is
 * colored on a cool→hot cost RAMP (from the D13 `/topology` price table) so the expensive lanes
 * read hot. Clicking a lane band (or its node) cross-links to the FlowTable filtered ATOMICALLY to
 * that band's `(upstream, model)` pair (finding 9) — the view owns the `$`/min readout.
 *
 * Lifecycle (§3.3 + `useImperativeViz`): d3 OWNS the <svg> (created in the imperative setup,
 * removed in cleanup), React owns the mount lifecycle. d3-sankey is a pure LAYOUT — no physics
 * timer — so "StrictMode-safe" here means the cleanup removes the SVG and the next mount rebuilds
 * exactly one (asserted by the test). The layout is recomputed when the model changes (new tokens
 * each ~1 s), which is cheap at realistic lane counts (≤ a handful of models).
 */
import { useEffect, useRef } from 'react';
import { sankey, sankeyLinkHorizontal, type SankeyGraph } from 'd3-sankey';
import { colors } from '../../design/tokens';
import { useImperativeViz, type VizCleanup } from '../../viz/useImperativeViz';
import { costColor, type SankeyModel } from './sankeyModel';
import { tokenSankeyCounters } from './tokenSankeyState';

const SVG_NS = 'http://www.w3.org/2000/svg';

/** The extra properties carried on d3-sankey nodes/links (beyond the layout-computed geometry). */
interface SNode { id: string; label: string; col: 0 | 1 | 2; model?: string; upstream?: string | null }
interface SLink { cost: number; model?: string; upstream?: string | null }

export interface TokenSankeyProps {
  model: SankeyModel;
  width?: number;
  height?: number;
  /** Click a lane band/node → cross-link filtered to that band's (upstream, model) pair (finding 9). */
  onSelectModel: (model: string, upstream: string | null) => void;
}

const DEFAULT_W = 720;
const DEFAULT_H = 420;
const MARGIN = 16;

export function TokenSankey({ model, width = DEFAULT_W, height = DEFAULT_H, onSelectModel }: TokenSankeyProps) {
  const ref = useRef<HTMLDivElement>(null);
  const modelRef = useRef(model);
  modelRef.current = model;
  const selectRef = useRef(onSelectModel);
  selectRef.current = onSelectModel;
  // Holds the live re-layout of the current SVG so a streaming token update re-renders in place.
  const renderRef = useRef<(() => void) | null>(null);

  useImperativeViz(
    ref,
    (el): VizCleanup => {
      tokenSankeyCounters.setups += 1;
      const svg = document.createElementNS(SVG_NS, 'svg');
      svg.setAttribute('data-testid', 'token-sankey-svg');
      svg.setAttribute('width', String(width));
      svg.setAttribute('height', String(height));
      svg.setAttribute('viewBox', `0 0 ${width} ${height}`);
      svg.setAttribute('role', 'img');
      svg.setAttribute('aria-label', 'Token-flow Sankey');
      svg.style.display = 'block';
      svg.style.maxWidth = '100%';
      const linkLayer = document.createElementNS(SVG_NS, 'g');
      linkLayer.setAttribute('data-layer', 'links');
      const nodeLayer = document.createElementNS(SVG_NS, 'g');
      nodeLayer.setAttribute('data-layer', 'nodes');
      svg.append(linkLayer, nodeLayer);
      el.appendChild(svg);

      const render = (): void => {
        const m = modelRef.current;
        linkLayer.replaceChildren();
        nodeLayer.replaceChildren();
        if (m.links.length === 0) return;

        // d3-sankey mutates its input — build fresh node/link objects each layout. With a `nodeId`
        // accessor set below, links reference nodes BY ID (string), not array index, so we pass the
        // ids straight through. client/gateway/model form a strict left→right DAG, so d3 derives
        // the three columns from the link topology.
        const nodes = m.nodes.map((n): SNode & { id: string } => ({ ...n }));
        const links = m.links.map((l) => ({
          source: l.source,
          target: l.target,
          value: l.value,
          cost: l.cost,
          model: l.model,
          upstream: l.upstream,
        }));
        const layout = sankey<SNode, SLink>()
          .nodeId((n) => n.id)
          .nodeWidth(14)
          .nodePadding(18)
          .extent([[MARGIN, MARGIN], [width - MARGIN, height - MARGIN]]);
        const graph: SankeyGraph<SNode, SLink> = layout({
          nodes: nodes as never,
          links: links as never,
        });

        const maxCost = Math.max(0, ...graph.links.map((l) => (l as unknown as SLink).cost));
        const linkPath = sankeyLinkHorizontal<SNode, SLink>();

        for (const link of graph.links) {
          const sl = link as unknown as SLink;
          const path = document.createElementNS(SVG_NS, 'path');
          const d = linkPath(link);
          if (d) path.setAttribute('d', d);
          path.setAttribute('fill', 'none');
          path.setAttribute('stroke', costColor(sl.cost, maxCost));
          path.setAttribute('stroke-opacity', '0.45');
          path.setAttribute('stroke-width', String(Math.max(1, link.width ?? 1)));
          path.setAttribute('data-testid', 'sankey-band');
          if (sl.model) path.setAttribute('data-model', sl.model);
          if (sl.upstream) path.setAttribute('data-upstream', sl.upstream);
          // Pulsing dash (CSS) along the band; gracefully static under reduced motion (the class
          // only animates when motion is allowed — see index.css).
          path.setAttribute('stroke-dasharray', '10 14');
          path.classList.add('sankey-band-flow');
          if (sl.model) {
            path.style.cursor = 'pointer';
            const model = sl.model;
            const upstream = sl.upstream ?? null;
            path.addEventListener('click', () => selectRef.current(model, upstream));
          }
          linkLayer.appendChild(path);
        }

        for (const node of graph.nodes) {
          const sn = node as unknown as SNode;
          const x0 = node.x0 ?? 0;
          const y0 = node.y0 ?? 0;
          const x1 = node.x1 ?? 0;
          const y1 = node.y1 ?? 0;
          const rect = document.createElementNS(SVG_NS, 'rect');
          rect.setAttribute('x', String(x0));
          rect.setAttribute('y', String(y0));
          rect.setAttribute('width', String(Math.max(1, x1 - x0)));
          rect.setAttribute('height', String(Math.max(1, y1 - y0)));
          rect.setAttribute('fill', sn.col === 2 ? colors.meta : sn.col === 1 ? colors.accent : colors.textMuted);
          rect.setAttribute('rx', '2');
          rect.setAttribute('data-testid', sn.col === 2 ? 'sankey-model-node' : 'sankey-node');
          rect.setAttribute('data-node-id', sn.id);
          if (sn.model) rect.setAttribute('data-model', sn.model);
          if (sn.upstream) rect.setAttribute('data-upstream', sn.upstream);
          if (sn.model) {
            rect.style.cursor = 'pointer';
            const model = sn.model;
            const upstream = sn.upstream ?? null;
            rect.addEventListener('click', () => selectRef.current(model, upstream));
          }
          nodeLayer.appendChild(rect);

          const label = document.createElementNS(SVG_NS, 'text');
          label.textContent = sn.label;
          const onRight = sn.col === 2;
          label.setAttribute('x', String(onRight ? x0 - 6 : x1 + 6));
          label.setAttribute('y', String((y0 + y1) / 2));
          label.setAttribute('dy', '0.35em');
          label.setAttribute('text-anchor', onRight ? 'end' : 'start');
          label.setAttribute('fill', colors.textMuted);
          label.setAttribute('font-size', '10');
          label.setAttribute('font-family', 'var(--font-ui)');
          nodeLayer.appendChild(label);
        }
      };

      renderRef.current = render;
      render();
      return () => {
        tokenSankeyCounters.cleanups += 1;
        renderRef.current = null;
        svg.remove();
      };
    },
    [width, height],
  );

  // Live re-layout: a streaming token update (same SVG, new band heights) recomputes in place
  // WITHOUT recreating the SVG — the Sankey twin of the Sparkline `setData` push. `renderRef` is
  // null on a StrictMode-discarded mount (already torn down), so it is guarded.
  useEffect(() => {
    renderRef.current?.();
  }, [model]);

  return <div ref={ref} data-testid="token-sankey" />;
}
