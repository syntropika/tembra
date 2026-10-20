const ErrorConstructor = Error;
const defineOwn = Object.defineProperty;

export function executorFailure(reason: string, message: string, code = "invalid"): Error {
  const error = new ErrorConstructor(message);
  defineOwn(error, "code", { value: code, enumerable: true, writable: true, configurable: true });
  defineOwn(error, "reason", {
    value: reason,
    enumerable: true,
    writable: true,
    configurable: true,
  });
  defineOwn(error, "detailsJson", {
    value: "null",
    enumerable: true,
    writable: true,
    configurable: true,
  });
  return error;
}
