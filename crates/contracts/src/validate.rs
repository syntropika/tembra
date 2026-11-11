//! Validate the deliberately bounded JSON value profile used by interpreted callbacks.
//!
//! A portable value has two spellings that differ only in WIT `u64`/`s64` positions. The native
//! kernel holds exact JSON numbers there ([`Form::Native`]), so ordinary Serde types keep their
//! `u64`/`i64` fields. The executor packet protocol, which JavaScript reads and writes, carries
//! canonical decimal strings there ([`Form::Wire`]), because `JSON.parse` rounds integers above
//! 2^53. Every other position, including WIT strings that hold digits, is identical in both.
use crate::{
    Error,
    model::{Function, FunctionKind, Item, Kind, MAX_DEPTH, PreparedContract, Slot, Ty},
};
use serde_json::Value;

/// How a portable value spells its WIT `u64`/`s64` positions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Form {
    /// Exact JSON numbers, as held by the native kernel and ordinary Serde.
    Native,
    /// Canonical decimal strings, as carried by the executor packet protocol.
    Wire,
}

impl PreparedContract {
    /// Find the complete interface identity; method spelling alone never selects a contract.
    fn locate(&self, binding: &str, method: &str, exports: bool) -> Option<u32> {
        self.slots()
            .iter()
            .position(|slot| {
                slot.exports == exports
                    && self.interface_matches(*slot, binding, exports)
                    && self.function(*slot).name == method
            })
            .map(|slot| slot as u32)
    }

    /// Exports are addressed by interface identity, imports by their world binding.
    fn interface_matches(&self, slot: Slot, name: &str, exports: bool) -> bool {
        let entry = &self.side(exports)[slot.entry as usize];
        match &entry.item {
            Item::Interface(interface) if exports => interface.contract.as_deref() == Some(name),
            Item::Interface(_) => entry.key == name,
            _ => false,
        }
    }

    /// Select the import or export side of the world.
    pub(crate) fn side(&self, exports: bool) -> &[crate::model::Entry] {
        if exports {
            &self.model().exports
        } else {
            &self.model().imports
        }
    }

    /// Read a bound signature.
    pub(crate) fn function(&self, slot: Slot) -> &Function {
        match &self.side(slot.exports)[slot.entry as usize].item {
            Item::Interface(interface) => &interface.functions[slot.function as usize],
            _ => unreachable!("slots only address interface functions"),
        }
    }

    /// Bind an interface method once; later calls validate through the returned slot.
    ///
    /// Exports are addressed by versioned interface identity and imports by world binding. The
    /// method is the original WIT name. A slot is only a lookup index into this contract.
    pub fn bind(&self, binding: &str, method: &str, exports: bool) -> Result<u32, Error> {
        self.locate(binding, method, exports)
            .ok_or_else(|| Error(format!("unknown WIT {} {binding}#{method}", side(exports))))
    }

    /// Validate a native argument tuple or result through a slot returned by `bind`.
    ///
    /// WIT `u64`/`s64` positions must hold exact JSON numbers; the wire spelling is refused.
    pub fn validate_slot(&self, slot: u32, arguments: bool, value: &Value) -> Result<(), Error> {
        self.check_slot(Form::Native, slot, arguments, value)
            .map(drop)
    }

    /// Validate an executor-packet argument tuple or result, whose WIT `u64`/`s64` positions
    /// must be canonical decimal strings; a JSON number there is refused.
    pub(crate) fn validate_wire(
        &self,
        slot: u32,
        arguments: bool,
        value: &Value,
    ) -> Result<(), Error> {
        self.check_slot(Form::Wire, slot, arguments, value)
            .map(drop)
    }

    /// Validate a native value and spell its WIT `u64`/`s64` positions as canonical decimal
    /// strings for the executor packet protocol. Nothing else changes.
    pub fn to_wire(&self, slot: u32, arguments: bool, value: Value) -> Result<Value, Error> {
        self.convert(Form::Native, slot, arguments, value)
    }

    /// Validate an executor-packet value and turn its WIT `u64`/`s64` decimal strings into the
    /// exact JSON numbers the native kernel holds. Nothing else changes.
    pub fn from_wire(&self, slot: u32, arguments: bool, value: Value) -> Result<Value, Error> {
        self.convert(Form::Wire, slot, arguments, value)
    }

    /// Check a value in `form` and return its function.
    fn check_slot(
        &self,
        form: Form,
        slot: u32,
        arguments: bool,
        value: &Value,
    ) -> Result<&Function, Error> {
        let slot = *self
            .slots()
            .get(slot as usize)
            .ok_or_else(|| Error("unknown WIT method slot".into()))?;
        let function = self.function(slot);
        if arguments {
            self.check_arguments(form, function, value)?;
        } else {
            self.check_result(form, function, value)?;
        }
        Ok(function)
    }

    /// Check a value in `from`, then respell only its 64-bit leaves in the other form. A
    /// signature without 64-bit integers returns the checked value untouched.
    fn convert(
        &self,
        from: Form,
        slot: u32,
        arguments: bool,
        mut value: Value,
    ) -> Result<Value, Error> {
        let function = self.check_slot(from, slot, arguments, &value)?;
        if arguments {
            let values = value.as_array_mut().ok_or_else(checked)?;
            for (parameter, value) in function.params.iter().zip(values) {
                self.respell(parameter.ty, value)?;
            }
        } else if let Some(ty) = function.result {
            self.respell(ty, &mut value)?;
        }
        Ok(value)
    }

    /// Flip the spelling of every `u64`/`s64` leaf in an already checked value, descending only
    /// into types that reach one (a per-type flag computed once at preparation). The check
    /// guaranteed every shape seen here; a disagreement is reported, never papered over.
    pub(crate) fn respell(&self, ty: Ty, value: &mut Value) -> Result<(), Error> {
        if !self.wide(ty) {
            return Ok(());
        }
        match ty {
            Ty::U64 => *value = flip::<u64>(value, Value::as_u64)?,
            Ty::S64 => *value = flip::<i64>(value, Value::as_i64)?,
            Ty::Ref(id) => match self.kind(id) {
                Kind::Alias(ty) => self.respell(*ty, value)?,
                Kind::Option(ty) => {
                    if !value.is_null() {
                        self.respell(*ty, value)?;
                    }
                }
                Kind::List(ty) => {
                    for value in value.as_array_mut().ok_or_else(checked)? {
                        self.respell(*ty, value)?;
                    }
                }
                Kind::Record(fields) => {
                    let object = value.as_object_mut().ok_or_else(checked)?;
                    for (field, spelling) in fields.iter().zip(self.spellings(id)) {
                        let value = object.get_mut(&**spelling).ok_or_else(checked)?;
                        self.respell(field.ty, value)?;
                    }
                }
                Kind::Tuple(types) => {
                    let values = value.as_array_mut().ok_or_else(checked)?;
                    for (ty, value) in types.iter().zip(values) {
                        self.respell(*ty, value)?;
                    }
                }
                Kind::Variant(cases) => {
                    let tag = value["tag"].as_str().ok_or_else(checked)?;
                    let case = cases.iter().find(|case| case.name == tag);
                    self.respell_payload(case.ok_or_else(checked)?.ty, value)?;
                }
                Kind::Result { ok, err } => {
                    let payload = match value["tag"].as_str() {
                        Some("ok") => *ok,
                        Some("err") => *err,
                        _ => return Err(checked()),
                    };
                    self.respell_payload(payload, value)?;
                }
                _ => {}
            },
            _ => {}
        }
        Ok(())
    }

    /// Respell the `val` payload of a checked variant or result.
    fn respell_payload(&self, ty: Option<Ty>, value: &mut Value) -> Result<(), Error> {
        match (ty, value.get_mut("val")) {
            (Some(ty), Some(payload)) => self.respell(ty, payload),
            (Some(_), None) => Err(checked()),
            (None, _) => Ok(()),
        }
    }

    /// Require every exported signature to use types supported by this portable profile.
    ///
    /// The incoming HTTP handler is not a portable export: its owned resources are served only
    /// by the host-local ingress adapter, so it is outside this profile rather than admitted to it.
    pub fn check_value_profile(&self) -> Result<(), Error> {
        self.check_exports(false)
    }

    /// Classify exports that are data plus a `stream<u8>` returned as the whole result of an
    /// asynchronous freestanding function.
    ///
    /// The stream end never travels as data: only the native facade's capability transport
    /// carries it, so data-only endpoints keep [`Self::check_value_profile`] and use this check
    /// only to explain that refusal. Stream arguments, nested streams, other element types,
    /// futures and resources do not match.
    pub fn check_stream_profile(&self) -> Result<(), Error> {
        self.check_exports(true)
    }

    fn check_exports(&self, streams: bool) -> Result<(), Error> {
        for entry in &self.model().exports {
            if crate::lower::is_ingress(entry) {
                continue;
            }
            if let Item::Interface(interface) = &entry.item {
                if !interface.resources.is_empty()
                    || interface
                        .functions
                        .iter()
                        .any(|function| !function.kind.freestanding())
                {
                    return Err(Error(
                        "resource exports require a capability transport".into(),
                    ));
                }
                for function in &interface.functions {
                    self.supported_function(function, streams)?;
                }
            }
        }
        Ok(())
    }

    /// Check an Orb import without applying the JSON profile to standard WASI resource imports.
    pub fn check_import_value_profile(&self, binding: &str) -> Result<(), Error> {
        self.check_import(binding, false)
    }

    /// Classify an Orb import that is data plus a `stream<u8>` returned as the whole result of an
    /// asynchronous freestanding function.
    ///
    /// As for [`Self::check_stream_profile`], only the native facade's capability transport
    /// carries the stream end; data-only endpoints keep [`Self::check_import_value_profile`].
    pub fn check_import_stream_profile(&self, binding: &str) -> Result<(), Error> {
        self.check_import(binding, true)
    }

    fn check_import(&self, binding: &str, streams: bool) -> Result<(), Error> {
        let entry = self
            .model()
            .imports
            .iter()
            .find(|entry| entry.key == binding)
            .ok_or_else(|| Error(format!("unknown WIT import {binding}")))?;
        let Item::Interface(interface) = &entry.item else {
            return Err(Error("Orb import must be an interface".into()));
        };
        if !interface.resources.is_empty()
            || interface
                .functions
                .iter()
                .any(|function| !function.kind.freestanding())
        {
            return Err(Error(
                "resource imports require a capability transport".into(),
            ));
        }
        for function in &interface.functions {
            self.supported_function(function, streams)?;
        }
        Ok(())
    }

    /// `streams` admits a `stream<u8>` returned as the whole result of an asynchronous
    /// freestanding function; every other position keeps the data profile.
    fn supported_function(&self, function: &Function, streams: bool) -> Result<(), Error> {
        for parameter in &function.params {
            self.supported(parameter.ty, 0)?;
        }
        if let Some(result) = function.result {
            if streams && self.byte_stream(result) {
                if !matches!(function.kind, FunctionKind::AsyncFreestanding) {
                    return Err(Error(
                        "a byte stream is carried only as the whole result of an asynchronous \
                         freestanding function"
                            .into(),
                    ));
                }
                return Ok(());
            }
            self.supported(result, 0)?;
        }
        Ok(())
    }

    /// Refuse lossy integer, native handle and asynchronous container coercions before execution.
    pub(crate) fn supported(&self, ty: Ty, depth: usize) -> Result<(), Error> {
        if depth > MAX_DEPTH {
            return Err(Error(
                "WIT type nesting exceeds the value profile limit".into(),
            ));
        }
        match ty {
            Ty::ErrorContext => Err(Error(
                "error contexts require a dedicated value codec".into(),
            )),
            Ty::Ref(id) => match self.kind(id) {
                Kind::Option(inner) => {
                    if let Ty::Ref(id) = self.resolved(*inner)
                        && matches!(self.kind(id), Kind::Option(_))
                    {
                        return Err(Error("nested options require a tagged option codec".into()));
                    }
                    self.supported(*inner, depth + 1)
                }
                Kind::Alias(inner) | Kind::List(inner) => self.supported(*inner, depth + 1),
                Kind::Record(fields) => fields
                    .iter()
                    .try_for_each(|field| self.supported(field.ty, depth + 1)),
                Kind::Tuple(types) => types
                    .iter()
                    .try_for_each(|ty| self.supported(*ty, depth + 1)),
                Kind::Variant(cases) => cases
                    .iter()
                    .filter_map(|case| case.ty)
                    .try_for_each(|ty| self.supported(ty, depth + 1)),
                Kind::Result { ok, err } => [*ok, *err]
                    .into_iter()
                    .flatten()
                    .try_for_each(|ty| self.supported(ty, depth + 1)),
                Kind::Flags(_) | Kind::Enum(_) => Ok(()),
                other => Err(Error(format!(
                    "{} requires a dedicated interpreter value adapter",
                    other.as_str()
                ))),
            },
            _ => Ok(()),
        }
    }

    /// Validate tuple length and each named parameter before invoking user code.
    fn check_arguments(
        &self,
        form: Form,
        function: &Function,
        arguments: &Value,
    ) -> Result<(), Error> {
        let method = &function.name;
        let values = arguments
            .as_array()
            .ok_or_else(|| Error("WIT arguments must be an array".into()))?;
        if values.len() != function.params.len() {
            return Err(Error(format!(
                "{method}: expected {} arguments",
                function.params.len()
            )));
        }
        for (parameter, value) in function.params.iter().zip(values) {
            self.validate(form, parameter.ty, value, 0)
                .map_err(|error| Error(format!("{method} argument {}: {error}", parameter.name)))?;
        }
        Ok(())
    }

    /// Validate the single WIT result or the absence of a result.
    pub(crate) fn check_result(
        &self,
        form: Form,
        function: &Function,
        value: &Value,
    ) -> Result<(), Error> {
        match function.result {
            Some(ty) => self.validate(form, ty, value, 0),
            None if value.is_null() => Ok(()),
            None => Err(Error("WIT function has no result".into())),
        }
    }

    /// Check values recursively without silently truncating fields or numeric widths.
    pub(crate) fn validate(
        &self,
        form: Form,
        ty: Ty,
        value: &Value,
        depth: usize,
    ) -> Result<(), Error> {
        if depth > MAX_DEPTH {
            return Err(Error("WIT value nesting exceeds its limit".into()));
        }
        // Feature unification may preserve `-0` as an integer Number. Check its floating
        // sign as well, so native and browser validators enforce the same profile.
        let integral = value.as_f64().is_some_and(finite);
        let unsigned = |limit: u64| integral && value.as_u64().is_some_and(|value| value <= limit);
        let signed = |min: i64, max: i64| {
            integral
                && value
                    .as_i64()
                    .is_some_and(|value| (min..=max).contains(&value))
        };
        let valid = match ty {
            Ty::Bool => value.is_boolean(),
            Ty::U8 => unsigned(u8::MAX.into()),
            Ty::U16 => unsigned(u16::MAX.into()),
            Ty::U32 => unsigned(u32::MAX.into()),
            Ty::S8 => signed(i8::MIN.into(), i8::MAX.into()),
            Ty::S16 => signed(i16::MIN.into(), i16::MAX.into()),
            Ty::S32 => signed(i32::MIN.into(), i32::MAX.into()),
            // Narrowing must not produce infinity or negative zero, which the guest cannot carry.
            Ty::F32 => value
                .as_f64()
                .is_some_and(|value| finite(value) && finite(f64::from(value as f32))),
            Ty::F64 => value.as_f64().is_some_and(finite),
            Ty::String => value.is_string(),
            Ty::Char => value
                .as_str()
                .is_some_and(|value| value.chars().count() == 1),
            // Native values hold exact integer numbers; the packet wire holds canonical strings.
            Ty::U64 => match form {
                Form::Native => integral && value.as_u64().is_some(),
                Form::Wire => canonical(value, false),
            },
            Ty::S64 => match form {
                Form::Native => integral && value.as_i64().is_some(),
                Form::Wire => canonical(value, true),
            },
            Ty::Ref(id) => return self.validate_kind(form, id, value, depth),
            Ty::ErrorContext => {
                return Err(Error("WIT type requires a dedicated value codec".into()));
            }
        };
        if valid {
            Ok(())
        } else {
            Err(Error(format!("value does not match {ty:?}")))
        }
    }

    /// Check a value against one type definition, using its cached member spellings.
    fn validate_kind(&self, form: Form, id: u32, value: &Value, depth: usize) -> Result<(), Error> {
        let kind = self.kind(id);
        let valid = match kind {
            Kind::Alias(ty) => return self.validate(form, *ty, value, depth + 1),
            Kind::List(ty) => {
                let values = value
                    .as_array()
                    .ok_or_else(|| Error("expected WIT list".into()))?;
                for value in values {
                    self.validate(form, *ty, value, depth + 1)?;
                }
                true
            }
            Kind::Record(fields) => {
                let object = value
                    .as_object()
                    .ok_or_else(|| Error("expected WIT record".into()))?;
                if object.len() != fields.len() {
                    return Err(Error("record fields differ from WIT".into()));
                }
                for (field, spelling) in fields.iter().zip(self.spellings(id)) {
                    let field_value = object
                        .get(&**spelling)
                        .ok_or_else(|| Error(format!("missing WIT field {}", field.name)))?;
                    self.validate(form, field.ty, field_value, depth + 1)?;
                }
                true
            }
            Kind::Tuple(types) => {
                let values = value
                    .as_array()
                    .filter(|values| values.len() == types.len())
                    .ok_or_else(|| Error("WIT tuple length differs".into()))?;
                for (ty, value) in types.iter().zip(values) {
                    self.validate(form, *ty, value, depth + 1)?;
                }
                true
            }
            Kind::Option(ty) => {
                if !value.is_null() {
                    self.validate(form, *ty, value, depth + 1)?;
                }
                true
            }
            Kind::Enum(cases) => value
                .as_str()
                .is_some_and(|value| cases.iter().any(|case| case == value)),
            Kind::Flags(_) => value.as_object().is_some_and(|object| {
                let flags = self.spellings(id);
                object.iter().all(|(key, value)| {
                    value.is_boolean() && flags.iter().any(|flag| **flag == **key)
                })
            }),
            Kind::Variant(cases) => {
                let tag = value["tag"]
                    .as_str()
                    .ok_or_else(|| Error("variant needs a tag".into()))?;
                let case = cases
                    .iter()
                    .find(|case| case.name == tag)
                    .ok_or_else(|| Error("unknown WIT variant case".into()))?;
                self.tagged(form, value, case.ty, depth)?;
                true
            }
            Kind::Result { ok, err } => {
                let ty = match value["tag"].as_str() {
                    Some("ok") => *ok,
                    Some("err") => *err,
                    _ => return Err(Error("result needs an ok or err tag".into())),
                };
                self.tagged(form, value, ty, depth)?;
                true
            }
            other => return Err(Error(format!("unsupported WIT value {}", other.as_str()))),
        };
        if valid {
            Ok(())
        } else {
            Err(Error(format!("value does not match WIT {}", kind.as_str())))
        }
    }

    /// Validate a tagged payload and refuse undeclared keys or missing payloads.
    fn tagged(&self, form: Form, value: &Value, ty: Option<Ty>, depth: usize) -> Result<(), Error> {
        let object = value
            .as_object()
            .ok_or_else(|| Error("expected tagged WIT object".into()))?;
        let expected = if ty.is_some() { 2 } else { 1 };
        if object.len() != expected {
            return Err(Error("tagged WIT fields differ".into()));
        }
        if let Some(ty) = ty {
            self.validate(
                form,
                ty,
                object
                    .get("val")
                    .ok_or_else(|| Error("missing WIT payload".into()))?,
                depth + 1,
            )?;
        }
        Ok(())
    }
}

/// Name a contract side in diagnostics.
fn side(exports: bool) -> &'static str {
    if exports { "export" } else { "import" }
}

/// Swap one checked 64-bit leaf between its exact JSON number and its canonical decimal string.
fn flip<T>(value: &Value, number: fn(&Value) -> Option<T>) -> Result<Value, Error>
where
    T: std::str::FromStr + ToString + Into<Value>,
{
    match value {
        Value::String(text) => text.parse::<T>().map(Into::into).map_err(|_| checked()),
        value => number(value)
            .map(|n| Value::String(n.to_string()))
            .ok_or_else(checked),
    }
}

/// A value disagreed with the shape its own check just admitted.
fn checked() -> Error {
    Error("a checked value changed shape during 64-bit conversion".into())
}

/// Whether `value` is the canonical decimal spelling of a 64-bit integer: no sign on `u64`, no
/// `+`, no `-0`, no leading zeros, no whitespace or exponent, and within range.
fn canonical(value: &Value, signed: bool) -> bool {
    let Some(text) = value.as_str() else {
        return false;
    };
    let digits = match text.strip_prefix('-') {
        Some(digits) if signed => digits,
        Some(_) => return false,
        None => text,
    };
    let shaped = match digits.as_bytes() {
        [b'0'] => digits.len() == text.len(),
        [b'1'..=b'9', rest @ ..] => rest.iter().all(u8::is_ascii_digit),
        _ => false,
    };
    shaped
        && if signed {
            text.parse::<i64>().is_ok()
        } else {
            text.parse::<u64>().is_ok()
        }
}

/// JSON cannot preserve negative zero between the two heaps.
fn finite(value: f64) -> bool {
    value.is_finite() && !(value == 0.0 && value.is_sign_negative())
}

/// Use the default JavaScript field spelling emitted by the official WIT bindings.
pub fn javascript_name(name: &str) -> String {
    heck::ToLowerCamelCase::to_lower_camel_case(name)
}

/// Corpus for the captured-text codec: the exact bytes produced by an encoder are what is checked.
#[cfg(all(test, feature = "authoring"))]
mod codec_tests {
    use crate::Contract;

    const SOURCE: &str = "package example:codec;
        interface values {
            enum mode { fast, slow }
            flags access { read, write-all }
            record item { name: string, count: option<u32> }
            variant shape { none, size(u32) }
            numbers: func(small: s32, wide: f64, narrow: f32, byte: u8) -> u32;
            items: func(values: list<item>) -> list<u8>;
            tagged: func(value: shape, mode: mode, access: access) -> result<string, string>;
            deep: func(value: list<list<list<u32>>>) -> char;
        }
        world run { export values; }";

    /// Bind once and check one argument tuple or result through the public text entry point.
    fn accepted(method: &str, arguments: bool, text: &str) -> bool {
        let contract = Contract::from_source("codec.wit", SOURCE, None).unwrap();
        let slot = contract.bind("example:codec/values", method, true).unwrap();
        let answer = contract.validate_packet(slot, arguments, text);
        assert!(
            answer == r#"{"ok":null}"# || answer.contains(r#""error""#),
            "{answer}"
        );
        answer == r#"{"ok":null}"#
    }

    #[test]
    fn numeric_text_preserves_the_portable_profile() {
        assert!(accepted("numbers", true, "[-2147483648,0.5,1.5,255]"));
        // Negative zero and fractional spellings of integers are refused, not normalized.
        assert!(!accepted("numbers", true, "[-0,0.5,1.5,1]"));
        assert!(!accepted("numbers", true, "[1,-0,1.5,1]"));
        assert!(!accepted("numbers", true, "[1.0,0.5,1.5,1]"));
        assert!(!accepted("numbers", true, "[1e2,0.5,1.5,1]"));
        // A negative value that rounds to negative zero or overflows as f32 is refused.
        assert!(!accepted("numbers", true, "[1,0.5,-1e-50,1]"));
        assert!(!accepted("numbers", true, "[1,0.5,1e39,1]"));
        assert!(accepted("numbers", true, "[1,0.5,1e-50,1]"));
        // JSON has no non-finite spelling; an overflowing literal is a decode error.
        assert!(!accepted("numbers", true, "[1,1e400,1.5,1]"));
        assert!(!accepted("numbers", true, "[1,NaN,1.5,1]"));
        // Wide integers never fit the 32-bit profile; f64 keeps the nearest double.
        assert!(!accepted("numbers", true, "[9007199254740993,0.5,1.5,1]"));
        assert!(!accepted(
            "numbers",
            true,
            "[18446744073709551616,0.5,1.5,1]"
        ));
        assert!(accepted("numbers", true, "[1,9007199254740993,1.5,1]"));
        assert!(!accepted("numbers", true, "[1,0.5,1.5,256]"));
        assert!(!accepted("numbers", false, "-1"));
        assert!(accepted("numbers", false, "4294967295"));
    }

    #[test]
    fn structural_text_requires_complete_portable_shapes() {
        assert!(accepted("items", true, r#"[[{"name":"a","count":null}]]"#));
        assert!(accepted("items", true, r#"[[{"name":"a","count":3}]]"#));
        // A missing option field stays refused rather than inferred as `none`.
        assert!(!accepted("items", true, r#"[[{"name":"a"}]]"#));
        assert!(!accepted(
            "items",
            true,
            r#"[[{"name":"a","count":null,"extra":1}]]"#
        ));
        // Typed arrays arrive as ordinary lists; their object spelling is not a list.
        assert!(accepted("items", false, "[0,255]"));
        assert!(!accepted("items", false, r#"{"0":0,"1":255}"#));
        assert!(!accepted("items", false, "[0,null]"));
        // Unpaired UTF-16 surrogates cannot become WIT strings.
        assert!(!accepted(
            "items",
            true,
            r#"[[{"name":"\ud800","count":null}]]"#
        ));
        assert!(!accepted("items", true, "[[]] trailing"));
    }

    #[test]
    fn tagged_text_and_depth_are_bounded() {
        let value = r#"[{"tag":"size","val":2},"fast",{"read":true,"writeAll":false}]"#;
        assert!(accepted("tagged", true, value));
        assert!(accepted("tagged", true, r#"[{"tag":"none"},"slow",{}]"#));
        assert!(!accepted(
            "tagged",
            true,
            r#"[{"tag":"none","val":null},"slow",{}]"#
        ));
        assert!(!accepted("tagged", true, r#"[{"tag":"size"},"slow",{}]"#));
        assert!(!accepted("tagged", true, r#"[{"tag":"none"},"other",{}]"#));
        assert!(!accepted(
            "tagged",
            true,
            r#"[{"tag":"none"},"slow",{"read":1}]"#
        ));
        assert!(!accepted(
            "tagged",
            true,
            r#"[{"tag":"none"},"slow",{"execute":true}]"#
        ));
        assert!(accepted("tagged", false, r#"{"tag":"err","val":"no"}"#));
        assert!(!accepted("tagged", false, r#"{"tag":"err"}"#));
        assert!(accepted("deep", true, "[[[[1]]]]"));
        assert!(!accepted("deep", true, "[[[[[1]]]]]"));
        assert!(accepted("deep", false, r#""😀""#));
        assert!(!accepted("deep", false, r#""ab""#));
        let nested = format!("{}{}", "[".repeat(512), "]".repeat(512));
        assert!(!accepted("deep", true, &nested));
    }
}
