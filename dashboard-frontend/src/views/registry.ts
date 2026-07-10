/** Route → view-component map. Kept out of the .tsx so react-refresh stays happy. */
import { lazy, type ComponentType } from 'react';
import type { RouteName } from '../router/useHashRoute';

// Every route is a dynamic boundary. In particular, d3-force/d3-sankey stay out of the
// authenticated shell's initial chunk and are fetched only when their views are opened.
const FlowsView = lazy(() => import('./FlowsView').then((module) => ({ default: module.FlowsView })));
const TopologyView = lazy(() => import('./TopologyView').then((module) => ({ default: module.TopologyView })));
const SankeyView = lazy(() => import('./SankeyView').then((module) => ({ default: module.SankeyView })));
const TheaterView = lazy(() => import('./TheaterView').then((module) => ({ default: module.TheaterView })));
const OverviewView = lazy(() =>
  import('./overview/OverviewView').then((module) => ({ default: module.OverviewView })),
);

export const VIEW_BY_ROUTE: Record<RouteName, ComponentType> = {
  flows: FlowsView,
  topology: TopologyView,
  sankey: SankeyView,
  theater: TheaterView,
  overview: OverviewView,
};
