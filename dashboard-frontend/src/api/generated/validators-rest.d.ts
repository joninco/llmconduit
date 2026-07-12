/* eslint-disable */
/** Generated CSP-safe standalone rest validator declarations. */
import type { CatalogEntry, FlowDetailBody, DurableFlowRollup, FlowsResponse, DurabilityStatusResponse, HistoryResponse, KillResponse, MetricsSnapshot, OverviewResponse, SnapshotResponse, TopologySnapshot, TheaterResponse } from './contracts';
import type { ContractValidator } from '../contractValidator';
export const validateCatalog: ContractValidator<CatalogEntry[]>;
export const validateFlowDetail: ContractValidator<FlowDetailBody>;
export const validateFlowSummary: ContractValidator<DurableFlowRollup>;
export const validateFlows: ContractValidator<FlowsResponse>;
export const validateDurability: ContractValidator<DurabilityStatusResponse>;
export const validateHistory: ContractValidator<HistoryResponse>;
export const validateKill: ContractValidator<KillResponse>;
export const validateMetrics: ContractValidator<MetricsSnapshot>;
export const validateOverview: ContractValidator<OverviewResponse>;
export const validateSnapshot: ContractValidator<SnapshotResponse>;
export const validateTopology: ContractValidator<TopologySnapshot>;
export const validateTheater: ContractValidator<TheaterResponse>;
