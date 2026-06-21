import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { StrictMode } from 'react';
import { render, cleanup, fireEvent } from '@testing-library/react';
import { RadialTopology } from './RadialTopology';
import { radialTopologyState, resetRadialTopologyState } from './radialTopologyState';
import { colors } from '../../design/tokens';
import type { ProviderHealth, TopologyEdge } from '../../api/types';

function provider(over: Partial<ProviderHealth>): ProviderHealth {
  return {
    id: 'p', name: 'p', route: null, base_url: 'http://x', status: 'healthy',
    cooling_until_ms: null, last_error: null, served_count: 0, failover_count: 0,
    consecutive_failures: 0, catalog_fetched_ms: null, catalog_size: 0, ...over,
  };
}

const NODES: ProviderHealth[] = [
  provider({ id: 'vllm-a', name: 'vllm-a', status: 'healthy' }),
  provider({ id: 'vllm-b', name: 'vllm-b', status: 'cooling', cooling_until_ms: Date.now() + 8000 }),
  provider({ id: 'openai', name: 'openai', status: 'down', last_error: '503' }),
];
const EDGES: TopologyEdge[] = [
  { from: 'gateway', to: 'vllm-a', throughput: 4, tokens_per_sec: 100, cost_per_sec: 0.003 },
];

beforeEach(() => {
  resetRadialTopologyState();
  cleanup();
});
afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe('RadialTopology — health-colored nodes from ProviderHealth', () => {
  it('colors each provider node by its D4 status (healthy/cooling/down)', () => {
    const { container } = render(
      <RadialTopology nodes={NODES} edges={EDGES} onSelectUpstream={() => {}} />,
    );
    const nodeEls = container.querySelectorAll('[data-testid="topo-node"]');
    expect(nodeEls.length).toBe(3);
    const fillFor = (id: string) =>
      container.querySelector(`[data-node-id="${id}"] circle`)?.getAttribute('fill');
    expect(fillFor('vllm-a')).toBe(colors.statusHealthy);
    expect(fillFor('vllm-b')).toBe(colors.statusCooling);
    expect(fillFor('openai')).toBe(colors.statusDown);
  });

  it('renders the gateway hub + client marker (client→gateway→providers)', () => {
    const { container } = render(<RadialTopology nodes={NODES} edges={EDGES} onSelectUpstream={() => {}} />);
    expect(container.querySelector('[data-testid="topo-gateway"]')).not.toBeNull();
    expect(container.querySelector('[data-testid="topo-client"]')).not.toBeNull();
  });
});

describe('RadialTopology — click → filter wiring', () => {
  it('clicking a provider node calls onSelectUpstream with its id', () => {
    const onSelect = vi.fn();
    const { container } = render(<RadialTopology nodes={NODES} edges={EDGES} onSelectUpstream={onSelect} />);
    const node = container.querySelector('[data-node-id="openai"]')!;
    fireEvent.click(node);
    expect(onSelect).toHaveBeenCalledWith('openai');
  });
});

describe('RadialTopology — cooldown tooltip hover reporting', () => {
  it('mouseenter on a node reports its provider ID + screen position via onHover (finding 7)', () => {
    const onHover = vi.fn();
    const { container } = render(<RadialTopology nodes={NODES} edges={EDGES} onSelectUpstream={() => {}} onHover={onHover} />);
    fireEvent.mouseEnter(container.querySelector('[data-node-id="vllm-b"]')!);
    expect(onHover).toHaveBeenCalledTimes(1);
    const arg = onHover.mock.calls[0]![0];
    // Reports the ID (not the health datum) so the view re-resolves CURRENT health by id.
    expect(arg.id).toBe('vllm-b');
    expect(typeof arg.x).toBe('number');
    expect(typeof arg.y).toBe('number');
    fireEvent.mouseLeave(container.querySelector('[data-node-id="vllm-b"]')!);
    expect(onHover).toHaveBeenLastCalledWith(null);
  });
});

describe('RadialTopology — live node-set changes rebuild the graph (finding 7)', () => {
  it('adding a provider renders a new node; removing one drops it', () => {
    const { container, rerender } = render(
      <RadialTopology nodes={NODES} edges={EDGES} onSelectUpstream={() => {}} />,
    );
    expect(container.querySelectorAll('[data-testid="topo-node"]').length).toBe(3);
    // Add a fourth provider → identity changes → the sim rebuilds with a node for it.
    const more = [...NODES, provider({ id: 'groq', name: 'groq', status: 'healthy' })];
    rerender(<RadialTopology nodes={more} edges={EDGES} onSelectUpstream={() => {}} />);
    expect(container.querySelectorAll('[data-testid="topo-node"]').length).toBe(4);
    expect(container.querySelector('[data-node-id="groq"]')).not.toBeNull();
    // Remove down to one → the dropped providers no longer linger.
    rerender(<RadialTopology nodes={[NODES[0]!]} edges={EDGES} onSelectUpstream={() => {}} />);
    expect(container.querySelectorAll('[data-testid="topo-node"]').length).toBe(1);
    expect(container.querySelector('[data-node-id="openai"]')).toBeNull();
  });

  it('a health-only change (same node set) recolors WITHOUT a full sim rebuild', () => {
    resetRadialTopologyState();
    const { container, rerender } = render(
      <RadialTopology nodes={NODES} edges={EDGES} onSelectUpstream={() => {}} />,
    );
    const setupsAfterMount = radialTopologyState.setups;
    // Flip vllm-a healthy → down (same ids/edges → identity unchanged → recolor only, no new setup).
    const recolored = [provider({ id: 'vllm-a', name: 'vllm-a', status: 'down' }), NODES[1]!, NODES[2]!];
    rerender(<RadialTopology nodes={recolored} edges={EDGES} onSelectUpstream={() => {}} />);
    expect(radialTopologyState.setups).toBe(setupsAfterMount); // no rebuild
    expect(container.querySelector('[data-node-id="vllm-a"] circle')?.getAttribute('fill')).toBe(colors.statusDown);
  });

  it('keeps the SAME node <g> element across a health update — hover target not destroyed (finding 5)', () => {
    const { container, rerender } = render(
      <RadialTopology nodes={NODES} edges={EDGES} onSelectUpstream={() => {}} />,
    );
    // Capture the live DOM node for vllm-a + arm an open hover on it.
    const before = container.querySelector('[data-node-id="vllm-a"]');
    expect(before).not.toBeNull();
    fireEvent.mouseEnter(before!);

    // A streaming health update (same node set) lands — the OLD code replaced every node <g>,
    // dropping the hover target; the keyed update reuses the SAME element in place.
    const recolored = [provider({ id: 'vllm-a', name: 'vllm-a', status: 'cooling', cooling_until_ms: Date.now() + 5000 }), NODES[1]!, NODES[2]!];
    rerender(<RadialTopology nodes={recolored} edges={EDGES} onSelectUpstream={() => {}} />);

    const after = container.querySelector('[data-node-id="vllm-a"]');
    // Identity preserved: the exact same DOM element (===), recolored in place.
    expect(after).toBe(before);
    expect(after!.querySelector('circle')?.getAttribute('fill')).toBe(colors.statusCooling);
    // And mouseleave still resolves on the preserved element (the listener was never torn down).
    const onHover = vi.fn();
    rerender(<RadialTopology nodes={recolored} edges={EDGES} onSelectUpstream={() => {}} onHover={onHover} />);
    fireEvent.mouseLeave(container.querySelector('[data-node-id="vllm-a"]')!);
    expect(onHover).toHaveBeenLastCalledWith(null);
  });
});

describe('RadialTopology — prefers-reduced-motion cuts particles', () => {
  it('emits edge particles when motion is allowed', () => {
    const { container } = render(<RadialTopology nodes={NODES} edges={EDGES} onSelectUpstream={() => {}} />);
    expect(container.querySelectorAll('[data-testid="topo-particle"]').length).toBeGreaterThan(0);
  });

  it('emits NO particle DOM under prefers-reduced-motion: reduce', () => {
    vi.stubGlobal('matchMedia', (query: string) => ({
      matches: query.includes('reduce'), media: query, onchange: null,
      addEventListener: () => {}, removeEventListener: () => {}, addListener: () => {}, removeListener: () => {}, dispatchEvent: () => false,
    }));
    const { container } = render(<RadialTopology nodes={NODES} edges={EDGES} onSelectUpstream={() => {}} />);
    expect(container.querySelectorAll('[data-testid="topo-particle"]').length).toBe(0);
    // The edge is still drawn (just not animated/flowing).
    expect(container.querySelectorAll('[data-testid="topo-edge"]').length).toBeGreaterThan(0);
  });
});

describe('RadialTopology — StrictMode-safe d3-force lifecycle (findings 7+8)', () => {
  it('double-invoke leaves ONE svg, stops every torn-down sim, clears its tick handler', () => {
    const { container, unmount } = render(
      <StrictMode>
        <RadialTopology nodes={NODES} edges={EDGES} onSelectUpstream={() => {}} />
      </StrictMode>,
    );
    // StrictMode mounts → unmounts → remounts: exactly ONE d3-owned SVG survives (no dup).
    expect(container.querySelectorAll('[data-testid="radial-topology-svg"]').length).toBe(1);
    // The discarded mount was torn down: setups ran ≥2, the first cleanup ran, stop() was CALLED.
    expect(radialTopologyState.setups).toBeGreaterThanOrEqual(2);
    expect(radialTopologyState.cleanups).toBeGreaterThanOrEqual(1);
    expect(radialTopologyState.stopCalls).toBe(radialTopologyState.cleanups);
    expect(radialTopologyState.allTickHandlersCleared).toBe(true);

    unmount();
    // After unmount the live sim is also torn down: fully balanced, stop() per setup, no leak.
    expect(radialTopologyState.cleanups).toBe(radialTopologyState.setups);
    expect(radialTopologyState.stopCalls).toBe(radialTopologyState.setups);
    expect(container.querySelectorAll('[data-testid="radial-topology-svg"]').length).toBe(0);
  });
});
