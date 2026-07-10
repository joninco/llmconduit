/** Structural shape shared by generated Ajv standalone validator modules. */
export interface ContractValidator<T> {
  (value: unknown): value is T;
  errors?: readonly unknown[] | null;
}
