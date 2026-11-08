// The outer host validates independently; these conversions only restore JavaScript shapes.
(() => {
  const contract = globalThis.__tembraContract;
  const types = new Map(contract.types.map(({ id, definition }) => [id, definition.kind]));
  const arrays = {
    u8: Uint8Array,
    s8: Int8Array,
    u16: Uint16Array,
    s16: Int16Array,
    u32: Uint32Array,
    s32: Int32Array,
    f32: Float32Array,
    f64: Float64Array,
  };

  /** Use the JavaScript spelling emitted by the official WIT type generator. */
  function name(value) {
    return value.replace(/-([a-z])/g, (_match, letter) => letter.toUpperCase());
  }

  /** Resolve aliases when deciding whether a list uses a numeric typed array. */
  function primitive(type) {
    while (typeof type === "number" && types.get(type)?.type !== undefined)
      type = types.get(type).type;
    return type;
  }

  /** Convert between bounded JSON values and the official JavaScript binding shapes. */
  function convert(type, value, lift) {
    if (typeof type !== "number") {
      if (type === "f32" || type === "f64") {
        if (typeof value !== "number" || !Number.isFinite(value) || Object.is(value, -0))
          throw new Error("WIT floats require a finite number without negative zero");
        const result = type === "f32" ? Math.fround(value) : value;
        if (!Number.isFinite(result) || Object.is(result, -0))
          throw new Error("WIT float cannot be represented by this value profile");
        return result;
      }
      if (
        /^[us](8|16|32)$/.test(type) &&
        (typeof value !== "number" || !Number.isInteger(value) || Object.is(value, -0))
      )
        throw new Error("WIT integers require an integer without negative zero");
      return value;
    }
    const kind = types.get(type);
    if (!kind) throw new Error("Unknown WIT type reference");
    if (kind.type !== undefined) return convert(kind.type, value, lift);
    if (kind.list !== undefined) {
      if (!Array.isArray(value) && !ArrayBuffer.isView(value))
        throw new Error("WIT list requires an array");
      const values = Array.from(value, (element) => convert(kind.list, element, lift));
      const ArrayType = arrays[primitive(kind.list)];
      return lift && ArrayType ? new ArrayType(values) : values;
    }
    if (kind.record) {
      const fields = kind.record.fields;
      if (
        !value ||
        typeof value !== "object" ||
        Object.keys(value).length !== fields.length ||
        fields.some((field) => !Object.hasOwn(value, name(field.name)))
      )
        throw new Error("WIT record fields differ");
      return Object.fromEntries(
        kind.record.fields.map((field) => [
          name(field.name),
          convert(field.type, value[name(field.name)], lift),
        ]),
      );
    }
    if (kind.tuple) {
      if (!Array.isArray(value) || value.length !== kind.tuple.types.length)
        throw new Error("WIT tuple length differs");
      return kind.tuple.types.map((type, index) => convert(type, value[index], lift));
    }
    if (kind.option !== undefined) {
      if (value === null || value === undefined) return lift ? undefined : null;
      return convert(kind.option, value, lift);
    }
    if (kind.variant || kind.result) {
      if (!value || typeof value !== "object" || typeof value.tag !== "string")
        throw new Error("WIT tagged value requires a tag");
      const payload = kind.variant
        ? kind.variant.cases.find((item) => item.name === value.tag)?.type
        : kind.result[value.tag];
      const valid = kind.variant
        ? kind.variant.cases.some((item) => item.name === value.tag)
        : ["ok", "err"].includes(value.tag);
      const hasPayload = payload !== undefined && payload !== null;
      if (
        !valid ||
        Object.keys(value).length !== (hasPayload ? 2 : 1) ||
        (hasPayload && !Object.hasOwn(value, "val"))
      )
        throw new Error("WIT tagged fields differ");
      return !hasPayload
        ? { tag: value.tag }
        : { tag: value.tag, val: convert(payload, value.val, lift) };
    }
    if (kind.flags || kind.enum) return value;
    throw new Error("WIT type has no interpreter value adapter");
  }

  /** Index signatures by their original interface ID and JavaScript method name. */
  function side(items, local) {
    return new Map(
      items
        .filter((item) => item.contract)
        .map((item) => [
          local ? name(item.contract.split("/").at(-1).split("@")[0]) : item.binding,
          new Map(
            Object.entries(item.functions).map(([method, signature]) => [name(method), signature]),
          ),
        ]),
    );
  }
  const exports = side(contract.exports, true);
  const imports = side(contract.imports, false);

  /** Reject missing signatures instead of guessing how a value should be interpreted. */
  function signature(side, service, method) {
    const result = side.get(service)?.get(method);
    if (!result) throw new Error(`Missing WIT signature for ${service}#${method}`);
    return result;
  }

  globalThis.__tembraValues = Object.freeze({
    arguments(service, method, args) {
      return signature(exports, service, method).params.map((param, index) =>
        convert(param.type, args[index], true),
      );
    },
    result(service, method, value) {
      const type = signature(exports, service, method).result;
      return type === undefined ? null : convert(type, value, false);
    },
    importArguments(service, method, args) {
      service = service.split("::").at(-1);
      const type = signature(imports, service, method);
      if (args.length !== type.params.length) throw new Error("WIT import argument count differs");
      return type.params.map((param, index) => convert(param.type, args[index], false));
    },
    importResult(service, method, value) {
      service = service.split("::").at(-1);
      const type = signature(imports, service, method).result;
      return type === undefined ? undefined : convert(type, value, true);
    },
  });
})();
