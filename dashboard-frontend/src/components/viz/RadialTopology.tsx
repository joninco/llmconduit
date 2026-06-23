/**
 * RadialTopology (D12) — the routing-story map. A radial hub-and-spoke laid out with d3-force:
 * the gateway hub at the center, each upstream provider on a ring around it, the client a small
 * marker behind the gateway (client → gateway → providers). Nodes are HEALTH-COLORED from the D4
 * `ProviderHealth.status` (healthy/cooling/down). Edges pulse and, when motion is allowed, carry
 * particles flowing OUTWARD along the edge (gateway → provider) at a rate ∝ the edge throughput;
 * `prefers-reduced-motion: reduce` disables the particles (static edges).
 *
 * Lifecycle discipline (§3.3 + `useImperativeViz`): d3 OWNS the <svg> (created/destroyed inside
 * the imperative setup); React owns the mount lifecycle. The `forceSimulation` is created in
 * `setup` and ALWAYS `stop()`ed (+ tick handler cleared) in the returned cleanup, so React 18
 * StrictMode's mount→unmount→remount can never leak a running sim or duplicate the SVG. The sim
 * runs to a settled radial layout then stops — the live animation is pure CSS on the rendered
 * SVG, so streaming `TopologyUpdate`s never restart a physics loop.
 *
 * Interaction: clicking a provider node calls `onSelectUpstream(node.id)` (the view wires that to
 * the shared FlowTable filter + navigation). Hovering a node reports its screen position + datum
 * up via `onHover`, so the VIEW renders the cooldown/last-error tooltip in React (d3 owns the
 * SVG; the tooltip is a sibling overlay, not injected into d3's tree).
 */
import { useEffect, useRef } from 'react';
import {
  forceSimulation,
  forceLink,
  forceManyBody,
  forceCenter,
  forceCollide,
  forceRadial,
  type Simulation,
  type SimulationNodeDatum,
  type SimulationLinkDatum,
} from 'd3-force';
import type { ProviderHealth, ProviderLatency, TopologyEdge } from '../../api/types';
import { colors, statusColor, prefersReducedMotion } from '../../design/tokens';
import { useImperativeViz, type VizCleanup } from '../../viz/useImperativeViz';
import { radialTopologyState } from './radialTopologyState';
import { providerNodeEmphasis } from './providerLatency';

const SVG_NS = 'http://www.w3.org/2000/svg';

/** Synthetic non-provider node ids (the hub + the client marker). */
const GATEWAY_ID = 'gateway';
const CLIENT_ID = '__client__';

/** A laid-out node: a provider (carries `health`) or one of the synthetic hub/client nodes. */
interface TopoNode extends SimulationNodeDatum {
  id: string;
  kind: 'gateway' | 'client' | 'provider';
  label: string;
  health?: ProviderHealth;
}
type TopoLink = SimulationLinkDatum<TopoNode>;

/**
 * What `onHover` reports: the provider's ID + its screen position (null = pointer left). The view
 * re-resolves the CURRENT `ProviderHealth` by this id on each render (finding 7), so an open tooltip
 * reflects streaming health updates instead of freezing the value captured at mouseenter time.
 */
export interface TopoHover {
  id: string;
  /** Center of the node in the SVG's client coordinate space. */
  x: number;
  y: number;
}

export interface RadialTopologyProps {
  nodes: ProviderHealth[];
  edges: TopologyEdge[];
  /**
   * Gap 13 — per-provider latency/error metrics keyed by provider id (the spec-12 `ProviderLatency`
   * from the REST/snapshot topology node). Drives per-node SIZE + an error-rate ring so a degrading
   * upstream stands out. A provider ABSENT from this map (no in-window samples) is NEUTRAL: base
   * size, no ring, its health color unchanged — never a `0`-sized or falsely-healthy node.
   */
  perProvider?: Record<string, ProviderLatency>;
  width?: number;
  height?: number;
  /** Click a provider node → cross-link to the FlowTable filtered to its upstream target. */
  onSelectUpstream: (upstreamId: string) => void;
  /** Hover a provider node (or null on leave) → the view renders the cooldown tooltip. */
  onHover?: (hover: TopoHover | null) => void;
}

const DEFAULT_W = 720;
const DEFAULT_H = 480;
const HUB_R = 18;
const CLIENT_R = 9;
const NODE_R = 14;

/** Throughput → stroke width (px), clamped so a hot edge can't dominate. */
function edgeWidth(throughput: number): number {
  return Math.max(1.5, Math.min(7, 1.5 + throughput * 0.9));
}

/**
 * A stable key for the graph IDENTITY — the node SET + edge SET, independent of health/throughput
 * (finding 7). Health/throughput changes recolor in place via `renderGraph` (no physics restart);
 * only an identity change (a provider or edge added/removed) re-runs `setup` to rebuild the sim, so
 * a freshly-discovered provider gets a node and a removed one stops lingering. Sorted so order is
 * irrelevant.
 */
function graphIdentity(providers: ProviderHealth[], edges: TopologyEdge[]): string {
  const nodeIds = providers.map((p) => p.id).sort().join(',');
  const edgeIds = edges.map((e) => `${e.from}->${e.to}`).sort().join(',');
  return `${nodeIds}|${edgeIds}`;
}

/** Build the node/link graph from the topology payload (synthetic client + gateway hub added). */
function buildGraph(providers: ProviderHealth[], edges: TopologyEdge[]): { nodes: TopoNode[]; links: TopoLink[] } {
  const nodes: TopoNode[] = [
    { id: CLIENT_ID, kind: 'client', label: 'client' },
    { id: GATEWAY_ID, kind: 'gateway', label: 'gateway' },
    ...providers.map((p): TopoNode => ({ id: p.id, kind: 'provider', label: p.name, health: p })),
  ];
  const byId = new Set(nodes.map((n) => n.id));
  const links: TopoLink[] = [{ source: CLIENT_ID, target: GATEWAY_ID }];
  // Gateway → provider edges from the topology, but ALSO ensure every provider is linked to the
  // hub even if no live edge exists yet (so a freshly-discovered, zero-throughput provider still
  // sits on the ring rather than drifting free).
  const linked = new Set<string>();
  for (const e of edges) {
    if (byId.has(e.from) && byId.has(e.to)) {
      links.push({ source: e.from, target: e.to });
      if (e.to !== GATEWAY_ID) linked.add(e.to);
      if (e.from !== GATEWAY_ID) linked.add(e.from);
    }
  }
  for (const p of providers) {
    if (!linked.has(p.id)) links.push({ source: GATEWAY_ID, target: p.id });
  }
  return { nodes, links };
}

export function RadialTopology({
  nodes,
  edges,
  perProvider,
  width = DEFAULT_W,
  height = DEFAULT_H,
  onSelectUpstream,
  onHover,
}: RadialTopologyProps) {
  const ref = useRef<HTMLDivElement>(null);
  // Keep the latest data + callbacks reachable from the (size/motion-keyed) setup without
  // re-running it: a streaming TopologyUpdate updates these refs and the live re-render pass
  // (the `dataRef` read on each frame) recolors nodes WITHOUT restarting the simulation. The
  // per-provider map rides here too (gap 13) so a refreshed metrics read re-sizes nodes in place.
  const dataRef = useRef({ nodes, edges, perProvider });
  dataRef.current = { nodes, edges, perProvider };
  const selectRef = useRef(onSelectUpstream);
  selectRef.current = onSelectUpstream;
  const hoverRef = useRef(onHover);
  hoverRef.current = onHover;
  // Holds the live `renderGraph` of the current SVG so a streaming TopologyUpdate can recolor
  // nodes/edges in place (no physics restart) — the topology twin of Sparkline's `setData`.
  const renderRef = useRef<(() => void) | null>(null);
  const reduced = prefersReducedMotion();
  // The graph IDENTITY (node/edge SET). Health/throughput changes do NOT change it (they recolor in
  // place); an added/removed provider or edge DOES, re-running setup to rebuild the sim (finding 7).
  const identity = graphIdentity(nodes, edges);

  // Recreate ONLY on a shape change (size/motion). Data changes flow through `dataRef` +
  // `renderGraph` below (no physics restart). d3 owns the <svg>; cleanup removes it + stops sim.
  useImperativeViz(
    ref,
    (el): VizCleanup => {
      radialTopologyState.setups += 1;

      const svg = document.createElementNS(SVG_NS, 'svg');
      svg.setAttribute('data-testid', 'radial-topology-svg');
      svg.setAttribute('width', String(width));
      svg.setAttribute('height', String(height));
      svg.setAttribute('viewBox', `0 0 ${width} ${height}`);
      svg.setAttribute('role', 'img');
      svg.setAttribute('aria-label', 'Provider topology');
      svg.style.display = 'block';
      svg.style.maxWidth = '100%';

      const edgeLayer = document.createElementNS(SVG_NS, 'g');
      edgeLayer.setAttribute('data-layer', 'edges');
      const nodeLayer = document.createElementNS(SVG_NS, 'g');
      nodeLayer.setAttribute('data-layer', 'nodes');
      svg.append(edgeLayer, nodeLayer);
      el.appendChild(svg);

      const cx = width / 2;
      const cy = height / 2;
      const ringR = Math.min(width, height) / 2 - NODE_R - 24;

      const { nodes: graphNodes, links } = buildGraph(dataRef.current.nodes, dataRef.current.edges);
      // Pin the client behind the hub and the hub at center so the ring radiates predictably.
      const hub = graphNodes.find((n) => n.id === GATEWAY_ID);
      if (hub) { hub.fx = cx; hub.fy = cy; }
      const client = graphNodes.find((n) => n.id === CLIENT_ID);
      if (client) { client.fx = cx; client.fy = cy + ringR + 4; }

      const sim: Simulation<TopoNode, TopoLink> = forceSimulation<TopoNode>(graphNodes)
        .force('link', forceLink<TopoNode, TopoLink>(links).id((n) => n.id).distance(ringR).strength(0.25))
        .force('charge', forceManyBody().strength(-340))
        .force('collide', forceCollide<TopoNode>(NODE_R + 6))
        .force('center', forceCenter(cx, cy))
        // Providers pulled onto the ring; hub/client are pinned so radial only shapes providers.
        .force('radial', forceRadial<TopoNode>((n) => (n.kind === 'provider' ? ringR : 0), cx, cy).strength((n) => (n.kind === 'provider' ? 0.85 : 0)));

      // Spy on stop() so the StrictMode test can assert it was actually called (deleting the
      // cleanup's stop() would drop the count and fail) — the d3 teardown contract. We do NOT store
      // the sim in a module-global (finding 8): a global handle would outlive the unmount, retaining
      // the stopped graph. The cleanup owns the only reference and tears it down.
      const realStop = sim.stop.bind(sim);
      sim.stop = () => {
        radialTopologyState.stopCalls += 1;
        return realStop();
      };

      // KEYED node elements (finding 5): each node's <g>/circle/label is created ONCE (by node id)
      // and UPDATED in place on every tick/data change — never destroyed+recreated. Destroying the
      // hovered node's <g> mid-hover (the old per-tick `replaceChildren`) dropped the hover target
      // and froze the tooltip at stale coords; a stable <g> keeps the pointer over the same element
      // and its mouseenter closure reads the live (sim-mutated) `node.x/node.y`. The node SET is
      // fixed for a setup lifetime (an add/remove bumps `identity` → a fresh setup), so this map is
      // built once here and reused across all ticks/updates.
      // `ring` (provider nodes only) is the gap-13 error-rate ring — a concentric stroked circle
      // shown ONLY for a `degrading` provider (elevated error rate). It is created up-front (hidden)
      // and toggled in place so a metrics refresh never recreates the node (preserves the hover).
      interface NodeEls { g: SVGGElement; circle: SVGCircleElement; ring?: SVGCircleElement; }
      const nodeEls = new Map<string, NodeEls>();
      const radiusOf = (node: TopoNode): number =>
        node.kind === 'gateway' ? HUB_R : node.kind === 'client' ? CLIENT_R : NODE_R;

      const createNode = (node: TopoNode): NodeEls => {
        const r = radiusOf(node);
        const g = document.createElementNS(SVG_NS, 'g');
        g.setAttribute('data-testid', node.kind === 'provider' ? 'topo-node' : `topo-${node.kind}`);
        g.setAttribute('data-node-id', node.id);

        // The main status circle is appended FIRST so it is the node's primary `<circle>` (the
        // status fill the topology colors by health — `querySelector('circle')` resolves to it).
        const circle = document.createElementNS(SVG_NS, 'circle');
        circle.setAttribute('r', String(r));
        circle.setAttribute('stroke', colors.bg);
        circle.setAttribute('stroke-width', '2');
        g.appendChild(circle);

        // Error-rate ring (provider nodes only, gap 13) — a stroked outline drawn AROUND the status
        // circle (`fill:none`, on top so it is a visible halo), shown ONLY for a `degrading`
        // provider. Created up-front (hidden) + toggled in place so a metrics refresh never recreates
        // the node (preserves the hover target — finding 5).
        let ring: SVGCircleElement | undefined;
        if (node.kind === 'provider') {
          ring = document.createElementNS(SVG_NS, 'circle');
          ring.setAttribute('fill', 'none');
          ring.setAttribute('stroke', colors.statusDown);
          ring.setAttribute('stroke-width', '2');
          ring.setAttribute('data-testid', 'topo-error-ring');
          ring.style.display = 'none';
          g.appendChild(ring);
        }

        const label = document.createElementNS(SVG_NS, 'text');
        label.textContent = node.label;
        label.setAttribute('y', String(r + 12));
        label.setAttribute('text-anchor', 'middle');
        label.setAttribute('fill', colors.textMuted);
        label.setAttribute('font-size', '10');
        label.setAttribute('font-family', 'var(--font-ui)');
        g.appendChild(label);

        // Provider nodes are interactive. Listeners attach ONCE (the <g> is never recreated), so a
        // tick/update mid-hover never tears down the listener nor the hover target — they read the
        // live `node.x/node.y` + `node.id` via closure (finding 5 + finding 7).
        if (node.kind === 'provider') {
          g.style.cursor = 'pointer';
          g.addEventListener('click', () => selectRef.current(node.id));
          // Report the provider ID (not the health datum) so the view re-resolves CURRENT health by
          // id on each render — an open tooltip then reflects streaming updates (finding 7).
          g.addEventListener('mouseenter', () => {
            const rect = svg.getBoundingClientRect();
            hoverRef.current?.({ id: node.id, x: rect.left + (node.x ?? 0), y: rect.top + (node.y ?? 0) });
          });
          g.addEventListener('mouseleave', () => hoverRef.current?.(null));
        }
        nodeLayer.appendChild(g);
        return { g, circle, ring };
      };

      // Render the SVG from the current node/link positions + the latest health data. Called on
      // each tick (layout settling) and whenever a live TopologyUpdate lands (recolor only). Edges +
      // particles are NOT hover targets, so the edge layer is rebuilt each call (particles are
      // transient and settle once the sim stops); the NODE layer is updated in place (keyed).
      const renderGraph = (): void => {
        const latest = dataRef.current;
        const healthById = new Map(latest.nodes.map((p) => [p.id, p]));
        const edgeById = new Map(latest.edges.map((e) => [`${e.from}->${e.to}`, e]));
        const perProviderMap = latest.perProvider ?? {};
        edgeLayer.replaceChildren();

        for (const link of links) {
          const s = link.source as TopoNode;
          const t = link.target as TopoNode;
          if (s.x == null || s.y == null || t.x == null || t.y == null) continue;
          const edge = edgeById.get(`${s.id}->${t.id}`);
          const throughput = edge?.throughput ?? 0;
          const line = document.createElementNS(SVG_NS, 'line');
          line.setAttribute('x1', String(s.x));
          line.setAttribute('y1', String(s.y));
          line.setAttribute('x2', String(t.x));
          line.setAttribute('y2', String(t.y));
          line.setAttribute('stroke', colors.line);
          line.setAttribute('stroke-width', String(edgeWidth(throughput)));
          line.setAttribute('stroke-linecap', 'round');
          line.setAttribute('data-testid', 'topo-edge');
          line.setAttribute('data-from', s.id);
          line.setAttribute('data-to', t.id);
          // Pulsing dash (CSS) when motion is allowed; static otherwise (reduced motion).
          if (!reduced && throughput > 0) {
            line.setAttribute('stroke-dasharray', '6 8');
            line.classList.add('topo-edge-flow');
          }
          edgeLayer.appendChild(line);

          // Particle flowing gateway → provider, count ∝ throughput. CSS animation drives it
          // along the edge; entirely skipped under reduced motion (acceptance: particles off).
          if (!reduced && t.kind === 'provider' && throughput > 0) {
            const count = Math.min(3, 1 + Math.floor(throughput / 2));
            for (let i = 0; i < count; i++) {
              const dot = document.createElementNS(SVG_NS, 'circle');
              dot.setAttribute('r', '2.4');
              dot.setAttribute('fill', colors.accent);
              dot.setAttribute('data-testid', 'topo-particle');
              dot.style.offsetPath = `path('M ${s.x} ${s.y} L ${t.x} ${t.y}')`;
              dot.style.animation = `topo-particle ${(2.4 / Math.max(0.5, throughput)).toFixed(2)}s linear ${(i * 0.5).toFixed(2)}s infinite`;
              edgeLayer.appendChild(dot);
            }
          }
        }

        for (const node of graphNodes) {
          if (node.x == null || node.y == null) continue;
          // Enter: create the keyed <g> once; Update: reuse it. Never recreate (preserves hover).
          let els = nodeEls.get(node.id);
          if (!els) {
            els = createNode(node);
            nodeEls.set(node.id, els);
          }
          // Update position in place (the sim mutates node.x/node.y each tick).
          els.g.setAttribute('transform', `translate(${node.x} ${node.y})`);

          const health = node.kind === 'provider' ? healthById.get(node.id) : undefined;
          const fill =
            node.kind === 'gateway' ? colors.accent
            : node.kind === 'client' ? colors.textMuted
            : statusColor(health?.status ?? 'down');
          els.circle.setAttribute('fill', fill);
          els.circle.setAttribute('data-status', health?.status ?? node.kind);
          // Cooling nodes pulse (CSS); toggle the class in place so a health change recolors without
          // recreating the element (and a no-longer-cooling node stops pulsing).
          els.circle.classList.toggle('topo-node-cooling', !reduced && health?.status === 'cooling');

          // Gap 13: SIZE/ring the node by its per-provider LATENCY + error emphasis (the spec-12
          // metrics, NOT the health status). An `unavailable` provider (no in-window samples) stays
          // NEUTRAL — base radius, no ring — never `0`-sized or recolored healthy. A `degrading`
          // provider (elevated error rate OR elevated p99 latency) is ENLARGED so it stands out; the
          // red error ring shows only for ERROR-driven degradation (a slow-but-no-errors provider is
          // enlarged + `data-latency-degraded` but carries no error ring — no failures to signal).
          if (node.kind === 'provider') {
            const emphasis = providerNodeEmphasis(perProviderMap[node.id]);
            const baseR = radiusOf(node);
            const scaledR = baseR * emphasis.sizeScale;
            els.circle.setAttribute('r', String(scaledR));
            els.g.setAttribute('data-emphasis', emphasis.state);
            els.g.setAttribute(
              'data-error-rate',
              emphasis.errorRatePct === null ? '' : String(emphasis.errorRatePct),
            );
            els.g.setAttribute('data-p99', emphasis.p99Ms === null ? '' : String(emphasis.p99Ms));
            els.g.setAttribute('data-latency-degraded', emphasis.latencyDegraded ? 'true' : 'false');
            if (els.ring) {
              if (emphasis.showErrorRing) {
                els.ring.setAttribute('r', String(scaledR + 3));
                els.ring.style.display = '';
              } else {
                els.ring.style.display = 'none';
              }
            }
          }
        }
      };

      renderRef.current = renderGraph;
      let ticks = 0;
      sim.on('tick', () => {
        ticks += 1;
        renderGraph();
      });
      // Render once immediately so a settled/zero-alpha sim (or jsdom, which never animates a
      // real RAF) still paints the graph deterministically for the tests.
      renderGraph();
      void ticks;

      return () => {
        radialTopologyState.cleanups += 1;
        renderRef.current = null;
        // REQUIRED teardown: stop the physics timer + drop the tick handler so no animation frame
        // survives the unmount, then remove the d3-owned SVG (StrictMode-safe). The sim has no
        // surviving reference outside this closure (finding 8), so it is fully released here.
        sim.stop();
        sim.on('tick', null);
        if (sim.on('tick') != null) radialTopologyState.allTickHandlersCleared = false;
        svg.remove();
      };
    },
    // `identity` re-runs setup when the node/edge SET changes (a provider/edge added/removed —
    // finding 7); size/motion changes also rebuild. Health/throughput-only changes flow through
    // `renderGraph` below (recolor, no physics restart).
    [width, height, reduced, identity],
  );

  // Live recolor: a streaming TopologyUpdate (same node set, changed health/throughput) OR a
  // refreshed per-provider metrics read (gap 13) re-renders the existing SVG from `dataRef` WITHOUT
  // recreating the simulation — the topology twin of the Sparkline `setData` push. `renderRef` is
  // null on a StrictMode-discarded mount (already torn down), so it is guarded. Runs after the
  // layout-effect setup on the same commit.
  useEffect(() => {
    renderRef.current?.();
  }, [nodes, edges, perProvider]);

  return <div ref={ref} data-testid="radial-topology" />;
}
