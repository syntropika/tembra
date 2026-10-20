/**
 * SDK validation refusal. reason is machine-readable; path identifies the failing value and
 * defaults to "/". Backend operations may also reject with structured runtime errors or other
 * thrown values, so callers should not assume every failure is a ContractError.
 */
export class ContractError extends Error {
  constructor(
    readonly reason: string,
    message: string,
    readonly path = "/",
  ) {
    super(`${reason}: ${path}: ${message}`);
    this.name = "ContractError";
  }
}
