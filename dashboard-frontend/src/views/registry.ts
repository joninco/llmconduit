/** Route → view-component map. Kept out of the .tsx so react-refresh stays happy. */
import type { ComponentType } from 'react';
import type { RouteName } from '../router/useHashRoute';
import { FlowsView } from './FlowsView';
import { TopologyView, SankeyView, TheaterView } from './placeholders';

export const VIEW_BY_ROUTE: Record<RouteName, ComponentType> = {
  flows: FlowsView,
  topology: TopologyView,
  sankey: SankeyView,
  theater: TheaterView,
};
