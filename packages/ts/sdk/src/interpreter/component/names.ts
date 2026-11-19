/**
 * The generated bindings' spelling of one WIT identifier: kebab-case words, each entirely lower
 * or upper case, joined in lower (functions, methods) or upper (resource classes) camel case.
 */
export function bindingName(name: string, upper: boolean): string {
  return name
    .split("-")
    .map((word, index) => {
      const lower = word.toLowerCase();
      return index === 0 && !upper ? lower : lower.charAt(0).toUpperCase() + lower.slice(1);
    })
    .join("");
}
