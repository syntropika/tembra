//! Deterministic bounded binary encoding for prepared contracts and signature digests.
use crate::{
    Error,
    model::{
        Case, Entry, Field, Function, FunctionKind, Interface, Item, Kind, MAX_ITEMS, MAX_MEMBERS,
        MAX_NAME, MAX_TYPES, Model, Ty,
    },
};

/// Format version of the compact contract body.
const VERSION: u8 = 1;

#[derive(Default)]
pub(crate) struct Writer(Vec<u8>);

impl Writer {
    pub(crate) fn finish(self) -> Vec<u8> {
        self.0
    }

    pub(crate) fn byte(&mut self, value: u8) {
        self.0.push(value);
    }

    /// Unsigned LEB128.
    pub(crate) fn u32(&mut self, mut value: u32) {
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            if value == 0 {
                self.0.push(byte);
                return;
            }
            self.0.push(byte | 0x80);
        }
    }

    pub(crate) fn len(&mut self, value: usize) {
        self.u32(u32::try_from(value).expect("bounded prepared length"));
    }

    pub(crate) fn str(&mut self, value: &str) {
        self.len(value.len());
        self.0.extend_from_slice(value.as_bytes());
    }

    pub(crate) fn bytes(&mut self, value: &[u8]) {
        self.len(value.len());
        self.0.extend_from_slice(value);
    }

    fn option_str(&mut self, value: Option<&str>) {
        match value {
            Some(value) => {
                self.byte(1);
                self.str(value);
            }
            None => self.byte(0),
        }
    }

    pub(crate) fn ty(&mut self, ty: Ty) {
        match ty {
            Ty::Ref(id) => {
                self.byte(0);
                self.u32(id);
            }
            primitive => self.byte(primitive_tag(primitive)),
        }
    }

    fn option_ty(&mut self, ty: Option<Ty>) {
        match ty {
            Some(ty) => {
                self.byte(1);
                self.ty(ty);
            }
            None => self.byte(0),
        }
    }
}

/// Bounded reader over untrusted bytes.
pub(crate) struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
}

/// Uniform diagnostic for any malformed encoding.
fn malformed() -> Error {
    Error("malformed prepared contract".into())
}

impl<'a> Reader<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    /// Require that every byte was consumed.
    pub(crate) fn finish(&self) -> Result<(), Error> {
        if self.position == self.bytes.len() {
            Ok(())
        } else {
            Err(Error("prepared contract has trailing bytes".into()))
        }
    }

    fn remaining(&self) -> usize {
        self.bytes.len() - self.position
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    pub(crate) fn position(&self) -> usize {
        self.position
    }

    /// WebAssembly LEB128, which permits padded encodings of at most five bytes.
    pub(crate) fn wasm_u32(&mut self) -> Result<u32, Error> {
        let mut value = 0_u32;
        for index in 0..5 {
            let byte = self.byte()?;
            let bits = u32::from(byte & 0x7f);
            if index == 4 && bits > 0x0f {
                return Err(malformed());
            }
            value |= bits << (7 * index);
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(malformed())
    }

    pub(crate) fn byte(&mut self) -> Result<u8, Error> {
        let byte = *self.bytes.get(self.position).ok_or_else(malformed)?;
        self.position += 1;
        Ok(byte)
    }

    /// Canonical unsigned LEB128: at most five bytes, no overflow and no redundant padding.
    pub(crate) fn u32(&mut self) -> Result<u32, Error> {
        let mut value = 0_u32;
        for index in 0..5 {
            let byte = self.byte()?;
            let bits = u32::from(byte & 0x7f);
            if index == 4 && bits > 0x0f {
                return Err(malformed());
            }
            value |= bits << (7 * index);
            if byte & 0x80 == 0 {
                if index > 0 && byte == 0 {
                    return Err(malformed());
                }
                return Ok(value);
            }
        }
        Err(malformed())
    }

    /// Read a count that cannot exceed its limit or the bytes left to describe it.
    pub(crate) fn count(&mut self, limit: usize) -> Result<usize, Error> {
        let count = self.u32()? as usize;
        if count > limit || count > self.remaining() {
            return Err(Error("prepared contract count exceeds its limit".into()));
        }
        Ok(count)
    }

    pub(crate) fn take(&mut self, length: usize) -> Result<&'a [u8], Error> {
        if length > self.remaining() {
            return Err(malformed());
        }
        let slice = &self.bytes[self.position..self.position + length];
        self.position += length;
        Ok(slice)
    }

    pub(crate) fn str(&mut self) -> Result<String, Error> {
        let length = self.u32()? as usize;
        if length > MAX_NAME {
            return Err(Error("prepared name exceeds its limit".into()));
        }
        String::from_utf8(self.take(length)?.to_vec()).map_err(|_| malformed())
    }

    fn flag(&mut self) -> Result<bool, Error> {
        match self.byte()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(malformed()),
        }
    }

    fn option_str(&mut self) -> Result<Option<String>, Error> {
        self.flag()?.then(|| self.str()).transpose()
    }

    fn ty(&mut self) -> Result<Ty, Error> {
        match self.byte()? {
            0 => Ok(Ty::Ref(self.u32()?)),
            tag => PRIMITIVES
                .iter()
                .find(|(candidate, _)| *candidate == tag)
                .map(|(_, ty)| *ty)
                .ok_or_else(malformed),
        }
    }

    fn option_ty(&mut self) -> Result<Option<Ty>, Error> {
        self.flag()?.then(|| self.ty()).transpose()
    }

    fn list<T>(
        &mut self,
        limit: usize,
        mut read: impl FnMut(&mut Self) -> Result<T, Error>,
    ) -> Result<Vec<T>, Error> {
        let count = self.count(limit)?;
        (0..count).map(|_| read(self)).collect()
    }
}

const PRIMITIVES: [(u8, Ty); 14] = [
    (1, Ty::Bool),
    (2, Ty::U8),
    (3, Ty::U16),
    (4, Ty::U32),
    (5, Ty::U64),
    (6, Ty::S8),
    (7, Ty::S16),
    (8, Ty::S32),
    (9, Ty::S64),
    (10, Ty::F32),
    (11, Ty::F64),
    (12, Ty::Char),
    (13, Ty::String),
    (14, Ty::ErrorContext),
];

/// Tag a primitive type.
fn primitive_tag(ty: Ty) -> u8 {
    PRIMITIVES
        .iter()
        .find(|(_, candidate)| *candidate == ty)
        .map(|(tag, _)| *tag)
        .expect("primitive type")
}

/// Encode a whole model.
pub(crate) fn write_model(writer: &mut Writer, model: &Model) {
    writer.byte(VERSION);
    writer.str(&model.world);
    writer.option_str(model.package.as_deref());
    writer.len(model.types.len());
    for kind in &model.types {
        write_kind(writer, kind);
    }
    for side in [&model.imports, &model.exports] {
        writer.len(side.len());
        for entry in side {
            writer.str(&entry.key);
            match &entry.item {
                Item::Interface(interface) => {
                    writer.byte(0);
                    writer.option_str(interface.contract.as_deref());
                    writer.option_str(interface.name.as_deref());
                    writer.byte(u8::from(interface.resources));
                    writer.len(interface.functions.len());
                    for function in &interface.functions {
                        write_function(writer, function);
                    }
                }
                Item::Function(function) => {
                    writer.byte(1);
                    write_function(writer, function);
                }
                Item::Type(ty) => {
                    writer.byte(2);
                    writer.ty(*ty);
                }
            }
        }
    }
}

fn write_kind(writer: &mut Writer, kind: &Kind) {
    let names = |writer: &mut Writer, values: &[String]| {
        writer.len(values.len());
        values.iter().for_each(|value| writer.str(value));
    };
    match kind {
        Kind::Alias(ty) => {
            writer.byte(0);
            writer.ty(*ty);
        }
        Kind::Record(fields) => {
            writer.byte(1);
            write_fields(writer, fields);
        }
        Kind::Tuple(types) => {
            writer.byte(2);
            writer.len(types.len());
            types.iter().for_each(|ty| writer.ty(*ty));
        }
        Kind::Variant(cases) => {
            writer.byte(3);
            writer.len(cases.len());
            for case in cases {
                writer.str(&case.name);
                writer.option_ty(case.ty);
            }
        }
        Kind::Enum(cases) => {
            writer.byte(4);
            names(writer, cases);
        }
        Kind::Flags(flags) => {
            writer.byte(5);
            names(writer, flags);
        }
        Kind::Option(ty) => {
            writer.byte(6);
            writer.ty(*ty);
        }
        Kind::Result { ok, err } => {
            writer.byte(7);
            writer.option_ty(*ok);
            writer.option_ty(*err);
        }
        Kind::List(ty) => {
            writer.byte(8);
            writer.ty(*ty);
        }
        Kind::FixedList(ty, length) => {
            writer.byte(9);
            writer.ty(*ty);
            writer.u32(*length);
        }
        Kind::Map(key, value) => {
            writer.byte(10);
            writer.ty(*key);
            writer.ty(*value);
        }
        Kind::Resource => writer.byte(11),
        Kind::Own(id) => {
            writer.byte(12);
            writer.u32(*id);
        }
        Kind::Borrow(id) => {
            writer.byte(13);
            writer.u32(*id);
        }
        Kind::Future(ty) => {
            writer.byte(14);
            writer.option_ty(*ty);
        }
        Kind::Stream(ty) => {
            writer.byte(15);
            writer.option_ty(*ty);
        }
    }
}

fn write_fields(writer: &mut Writer, fields: &[Field]) {
    writer.len(fields.len());
    for field in fields {
        writer.str(&field.name);
        writer.ty(field.ty);
    }
}

/// Encode a function kind tag and its resource scope.
pub(crate) fn write_function_kind(writer: &mut Writer, kind: FunctionKind) {
    let (tag, resource) = match kind {
        FunctionKind::Freestanding => (0, None),
        FunctionKind::AsyncFreestanding => (1, None),
        FunctionKind::Getter => (2, None),
        FunctionKind::Setter => (3, None),
        FunctionKind::Method(id) => (4, Some(id)),
        FunctionKind::AsyncMethod(id) => (5, Some(id)),
        FunctionKind::MethodGetter(id) => (6, Some(id)),
        FunctionKind::MethodSetter(id) => (7, Some(id)),
        FunctionKind::Static(id) => (8, Some(id)),
        FunctionKind::AsyncStatic(id) => (9, Some(id)),
        FunctionKind::StaticGetter(id) => (10, Some(id)),
        FunctionKind::StaticSetter(id) => (11, Some(id)),
        FunctionKind::Constructor(id) => (12, Some(id)),
    };
    writer.byte(tag);
    if let Some(id) = resource {
        writer.u32(id);
    }
}

fn write_function(writer: &mut Writer, function: &Function) {
    writer.str(&function.name);
    write_function_kind(writer, function.kind);
    write_fields(writer, &function.params);
    writer.option_ty(function.result);
}

/// Decode a whole model; semantic checks happen in `Model::check`.
pub(crate) fn model(reader: &mut Reader<'_>) -> Result<Model, Error> {
    if reader.byte()? != VERSION {
        return Err(Error("unsupported prepared contract version".into()));
    }
    let world = reader.str()?;
    let package = reader.option_str()?;
    let types = reader.list(MAX_TYPES, kind)?;
    let imports = reader.list(MAX_ITEMS, entry)?;
    let exports = reader.list(MAX_ITEMS, entry)?;
    Ok(Model {
        world,
        package,
        types,
        imports,
        exports,
    })
}

fn entry(reader: &mut Reader<'_>) -> Result<Entry, Error> {
    let key = reader.str()?;
    let item = match reader.byte()? {
        0 => Item::Interface(Interface {
            contract: reader.option_str()?,
            name: reader.option_str()?,
            resources: reader.flag()?,
            functions: reader.list(MAX_MEMBERS, function)?,
        }),
        1 => Item::Function(function(reader)?),
        2 => Item::Type(reader.ty()?),
        _ => return Err(malformed()),
    };
    Ok(Entry { key, item })
}

fn fields(reader: &mut Reader<'_>) -> Result<Vec<Field>, Error> {
    reader.list(MAX_MEMBERS, |reader| {
        Ok(Field {
            name: reader.str()?,
            ty: reader.ty()?,
        })
    })
}

fn function(reader: &mut Reader<'_>) -> Result<Function, Error> {
    let name = reader.str()?;
    let tag = reader.byte()?;
    let kind = match tag {
        0 => FunctionKind::Freestanding,
        1 => FunctionKind::AsyncFreestanding,
        2 => FunctionKind::Getter,
        3 => FunctionKind::Setter,
        4..=12 => {
            let id = reader.u32()?;
            match tag {
                4 => FunctionKind::Method(id),
                5 => FunctionKind::AsyncMethod(id),
                6 => FunctionKind::MethodGetter(id),
                7 => FunctionKind::MethodSetter(id),
                8 => FunctionKind::Static(id),
                9 => FunctionKind::AsyncStatic(id),
                10 => FunctionKind::StaticGetter(id),
                11 => FunctionKind::StaticSetter(id),
                _ => FunctionKind::Constructor(id),
            }
        }
        _ => return Err(malformed()),
    };
    Ok(Function {
        name,
        kind,
        params: fields(reader)?,
        result: reader.option_ty()?,
    })
}

fn kind(reader: &mut Reader<'_>) -> Result<Kind, Error> {
    let names = |reader: &mut Reader<'_>| reader.list(MAX_MEMBERS, Reader::str);
    Ok(match reader.byte()? {
        0 => Kind::Alias(reader.ty()?),
        1 => Kind::Record(fields(reader)?),
        2 => Kind::Tuple(reader.list(MAX_MEMBERS, Reader::ty)?),
        3 => Kind::Variant(reader.list(MAX_MEMBERS, |reader| {
            Ok(Case {
                name: reader.str()?,
                ty: reader.option_ty()?,
            })
        })?),
        4 => Kind::Enum(names(reader)?),
        5 => Kind::Flags(names(reader)?),
        6 => Kind::Option(reader.ty()?),
        7 => Kind::Result {
            ok: reader.option_ty()?,
            err: reader.option_ty()?,
        },
        8 => Kind::List(reader.ty()?),
        9 => Kind::FixedList(reader.ty()?, reader.u32()?),
        10 => Kind::Map(reader.ty()?, reader.ty()?),
        11 => Kind::Resource,
        12 => Kind::Own(reader.u32()?),
        13 => Kind::Borrow(reader.u32()?),
        14 => Kind::Future(reader.option_ty()?),
        15 => Kind::Stream(reader.option_ty()?),
        _ => return Err(malformed()),
    })
}
