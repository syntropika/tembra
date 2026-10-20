//! Captured browser primitives shared by Context issuance and foreign callback observation.

use wasm_bindgen::{JsValue, prelude::wasm_bindgen};

#[wasm_bindgen(inline_js = r#"
const P = Promise, resolve = Promise.resolve, then = Promise.prototype.then;
const apply = Reflect.apply, freeze = Object.freeze, keys = Object.keys;
const stringify = JSON.stringify, parse = JSON.parse, ownKeys = Reflect.ownKeys;
const descriptor = Object.getOwnPropertyDescriptor, prototype = Object.getPrototypeOf;
const hasOwn = Object.hasOwn;
const isArray = Array.isArray, objectPrototype = Object.prototype, arrayPrototype = Array.prototype;
const finite = Number.isFinite, integer = Number.isInteger, safeInteger = Number.isSafeInteger;
const SetConstructor = Set, has = Set.prototype.has, add = Set.prototype.add;
const remove = Set.prototype.delete;
const charCode = String.prototype.charCodeAt, string = String;
export function observe_owned(value) {
  const promise = apply(resolve, P, [value]);
  return new P((ok, fail) => apply(then, promise, [ok, fail]));
}
export function fresh_context_token() { return freeze({}); }
export function object_keys_owned(value) { return keys(value); }
function portable_failure(reason, message) {
  throw {code:'invalid', reason, message, detailsJson:'null'};
}
function scalar_string(value) {
  for (let index = 0; index < value.length; index++) {
    const point = apply(charCode, value, [index]);
    if (point >= 0xd800 && point <= 0xdbff) {
      const next = apply(charCode, value, [++index]);
      if (!(next >= 0xdc00 && next <= 0xdfff))
        portable_failure('unicode', 'unpaired high surrogate');
    } else if (point >= 0xdc00 && point <= 0xdfff)
      portable_failure('unicode', 'unpaired low surrogate');
  }
}
export function portable_json_owned(value) {
  const active = new SetConstructor();
  function bounded(text) {
    if (text.length > 1048576) portable_failure('json_size', 'portable input exceeds one MiB');
    return text;
  }
  function capture(value, depth) {
    if (depth > 128) portable_failure('depth', 'portable value nesting limit exceeded');
    if (value === null || typeof value === 'boolean') return stringify(value);
    if (typeof value === 'string') {
      bounded(value); scalar_string(value); return bounded(stringify(value));
    }
    if (typeof value === 'number') {
      if (!finite(value) || (integer(value) && !safeInteger(value)))
        portable_failure('number',
          'number must be finite and integral values must be safe integers');
      return stringify(value);
    }
    if (typeof value !== 'object') portable_failure('portable', 'value is not strict JSON');
    if (depth >= 128) portable_failure('depth', 'portable value nesting limit exceeded');
    if (apply(has, active, [value])) portable_failure('portable', 'cyclic value');
    const array = isArray(value), proto = prototype(value);
    if (array ? proto !== arrayPrototype : proto !== objectPrototype && proto !== null)
      portable_failure('portable', 'native objects are not portable');
    apply(add, active, [value]);
    let text = array ? '[' : '{';
    const names = ownKeys(value);
    if (array) {
      const length = descriptor(value, 'length');
      if (!length || !hasOwn(length, 'value') || !safeInteger(length.value) || length.value < 0 ||
          names.length !== length.value + 1)
        portable_failure('portable', 'arrays must be dense and have no custom properties');
      for (let index = 0; index < length.value; index++) {
        const slot = descriptor(value, string(index));
        if (!slot || !hasOwn(slot, 'value') || !slot.enumerable)
          portable_failure('portable', 'array element is missing or an accessor');
        text = bounded(text + (index ? ',' : '') + capture(slot.value, depth + 1));
      }
    } else {
      for (let index = 0; index < names.length; index++) {
        const name = names[index];
        if (typeof name !== 'string') portable_failure('portable', 'symbol property');
        bounded(name); scalar_string(name);
        const slot = descriptor(value, name);
        if (!slot || !hasOwn(slot, 'value') || !slot.enumerable)
          portable_failure('portable', 'accessors and hidden properties are not portable');
        text = bounded(text + (index ? ',' : '') + stringify(name) + ':' +
          capture(slot.value, depth + 1));
      }
    }
    apply(remove, active, [value]);
    return bounded(text + (array ? ']' : '}'));
  }
  return capture(value, 0);
}
export function parse_portable_owned(text) { return parse(text); }
export function reject_async(value) {
  if (value !== null && (typeof value === 'object' || typeof value === 'function') &&
      typeof value.then === 'function')
    throw new TypeError('synchronous callback returned a promise');
  return value;
}
"#)]
extern "C" {
    #[wasm_bindgen(catch)]
    pub(crate) fn observe_owned(value: &JsValue) -> Result<js_sys::Promise, JsValue>;
    pub(crate) fn fresh_context_token() -> JsValue;
    #[wasm_bindgen(catch)]
    pub(crate) fn object_keys_owned(value: &JsValue) -> Result<js_sys::Array, JsValue>;
    #[wasm_bindgen(catch)]
    pub(crate) fn portable_json_owned(value: &JsValue) -> Result<String, JsValue>;
    #[wasm_bindgen(catch)]
    pub(crate) fn parse_portable_owned(text: &str) -> Result<JsValue, JsValue>;
    #[wasm_bindgen(catch)]
    pub(crate) fn reject_async(value: &JsValue) -> Result<JsValue, JsValue>;
}
