import type { ContractValidator } from './contractValidator';

export class DashboardContractError extends Error {
  constructor(
    readonly contract: string,
    readonly validationErrors: readonly unknown[] | null | undefined,
  ) {
    super(`dashboard contract validation failed: ${contract}`);
    this.name = 'DashboardContractError';
  }
}

/** Validate an untrusted decoded root before it reaches a cache or store. */
export function assertContract<T>(
  contract: string,
  validator: ContractValidator<T>,
  value: unknown,
): T {
  if (!validator(value)) {
    throw new DashboardContractError(contract, validator.errors);
  }
  return value;
}
