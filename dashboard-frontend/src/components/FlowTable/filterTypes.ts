/** Filter model for the FlowTable, kept out of the component file so react-refresh stays happy. */
import type { FlowStatus } from '../../api/types';

export interface FlowFilters {
  status: FlowStatus | null;
  model: string | null;
  upstream: string | null;
}

export const EMPTY_FILTERS: FlowFilters = { status: null, model: null, upstream: null };
