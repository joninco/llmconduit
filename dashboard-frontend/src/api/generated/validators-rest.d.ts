/* eslint-disable */
/** Generated CSP-safe standalone rest validator declarations. */
import type { CatalogEntry, FlowDetailBody, FlowsResponse, KillResponse, MetricsSnapshot, OverviewResponse, SnapshotResponse, TopologySnapshot } from './contracts';
import type { ContractValidator } from '../contractValidator';
export const validateCatalog: ContractValidator<CatalogEntry[]>;
export const validateFlowDetail: ContractValidator<FlowDetailBody>;
export const validateFlows: ContractValidator<FlowsResponse>;
export const validateKill: ContractValidator<KillResponse>;
export const validateMetrics: ContractValidator<MetricsSnapshot>;
export const validateOverview: ContractValidator<OverviewResponse>;
export const validateSnapshot: ContractValidator<SnapshotResponse>;
export const validateTopology: ContractValidator<TopologySnapshot>;
