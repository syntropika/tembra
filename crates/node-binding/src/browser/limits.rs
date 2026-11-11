//! Bounded execution for the core modules of a prepared browser component.
//!
//! A browser cannot preempt WebAssembly on the main thread, so a guest loop would block every
//! timer. At preparation time (native tooling, never shipped to browsers) each core module of a
//! verified component is re-encoded, through the official `wasm_encoder::reencode`, to import two
//! private mutable `i64` globals from `tembra:limits`:
//!
//! - `fuel`: every function entry and every `loop` header traps when it is zero and otherwise
//!   decrements it, so every unbounded execution (loops and tail calls alike) ends in a trap;
//! - `pages`: every `memory.grow` returns -1 unless the remaining 64 KiB pages cover it, and debits
//!   them when it succeeds.
//!
//! The browser host shares both globals between all core modules of one component instance, and
//! debits [`InstrumentedCore::initial_pages`] on every instantiation of the module. Defined
//! memories are clamped to the preparation maximum; the per-installation budget is the `pages`
//! global. Only linear memory is bounded: tables, the GC heap and host allocations are not. Shapes
//! that cannot be counted exactly are refused, never approximated.
use wasm_encoder::reencode::{Error, Reencode, utils};
use wasm_encoder::{BlockType, CodeSection, EntityType, Function, GlobalType, ImportSection};
use wasm_encoder::{Instruction, MemorySection, Module, SectionId, ValType};
use wasmparser::{CompositeInnerType, FunctionBody, MemoryType, Operator, Parser, Payload};
use wasmparser::{TypeRef, Validator, WasmFeatures};

/// Import module of the private limit globals; an input that already uses it is refused.
pub const LIMITS_MODULE: &str = "tembra:limits";
/// The only supported memory page size.
const PAGE_SIZE_LOG2: u32 = 16;
/// Appended imported globals: `fuel`, then `pages`.
const ADDED_GLOBALS: u32 = 2;

/// A core module that can only run while its instance's fuel and page budget allow.
pub struct InstrumentedCore {
    /// The validated module to compile in place of the generated one.
    pub bytes: Vec<u8>,
    /// Pages that every instantiation of this module allocates for the memories it defines.
    pub initial_pages: u64,
}

/// The supported input set: WebAssembly 3.0, which excludes shared memories and custom pages.
fn features() -> WasmFeatures {
    WasmFeatures::WASM3
}

pub(crate) fn instrument(bytes: &[u8], maximum_pages: u64) -> Result<InstrumentedCore, String> {
    let shape = Shape::read(bytes, maximum_pages)?;
    Validator::new_with_features(features())
        .validate_all(bytes)
        .map_err(|error| format!("invalid core module: {error}"))?;
    let mut encoder = Limits {
        initial_pages: 0,
        shape,
        maximum_pages,
        next_function: 0,
        imports_added: false,
    };
    let mut module = Module::new();
    encoder
        .parse_core_module(&mut module, Parser::new(0), bytes)
        .map_err(|error| match error {
            Error::UserError(message) => message,
            error => format!("cannot instrument core module: {error}"),
        })?;
    if !encoder.imports_added {
        return Err("limit imports were not added".into());
    }
    let bytes = module.finish();
    Validator::new_with_features(features())
        .validate_all(&bytes)
        .map_err(|error| format!("instrumented core module is invalid: {error}"))?;
    Ok(InstrumentedCore {
        bytes,
        initial_pages: encoder.initial_pages,
    })
}

/// Index spaces and memory shapes the rewrite depends on, read before re-encoding.
#[derive(Default)]
struct Shape {
    has_imports: bool,
    imported_globals: u32,
    /// Parameter count per type index (`None` for non-function types).
    params: Vec<Option<u32>>,
    /// Type index of each defined function, in code order.
    functions: Vec<u32>,
    /// Whether each memory (imported, then defined) uses 64-bit addresses.
    memory64: Vec<bool>,
}

impl Shape {
    fn read(bytes: &[u8], maximum_pages: u64) -> Result<Self, String> {
        let mut shape = Shape::default();
        for payload in Parser::new(0).parse_all(bytes) {
            match payload.map_err(|error| error.to_string())? {
                Payload::Version {
                    encoding: wasmparser::Encoding::Component,
                    ..
                } => {
                    return Err("expected a core module, found a component".into());
                }
                Payload::TypeSection(reader) => {
                    for group in reader {
                        for ty in group.map_err(|error| error.to_string())?.into_types() {
                            shape.params.push(match &ty.composite_type.inner {
                                CompositeInnerType::Func(func) => Some(func.params().len() as u32),
                                _ => None,
                            });
                        }
                    }
                }
                Payload::ImportSection(reader) => {
                    shape.has_imports = true;
                    for import in reader.into_imports() {
                        let import = import.map_err(|error| error.to_string())?;
                        if import.module == LIMITS_MODULE {
                            return Err(format!("import module `{LIMITS_MODULE}` is reserved"));
                        }
                        match import.ty {
                            TypeRef::Global(_) => shape.imported_globals += 1,
                            TypeRef::Memory(memory) => shape.memory(memory, None)?,
                            _ => {}
                        }
                    }
                }
                Payload::FunctionSection(reader) => {
                    for ty in reader {
                        shape.functions.push(ty.map_err(|error| error.to_string())?);
                    }
                }
                Payload::MemorySection(reader) => {
                    for memory in reader {
                        shape.memory(
                            memory.map_err(|error| error.to_string())?,
                            Some(maximum_pages),
                        )?;
                    }
                }
                _ => {}
            }
        }
        Ok(shape)
    }

    /// Refuse memories whose growth cannot be counted in exact 64 KiB pages.
    fn memory(&mut self, memory: MemoryType, maximum: Option<u64>) -> Result<(), String> {
        if memory.shared {
            return Err("shared memories are not supported".into());
        }
        if memory
            .page_size_log2
            .is_some_and(|log2| log2 != PAGE_SIZE_LOG2)
        {
            return Err("custom memory page sizes are not supported".into());
        }
        if let Some(maximum) = maximum
            && memory.initial > maximum
        {
            return Err(format!(
                "memory needs {} pages, more than the limit of {maximum}",
                memory.initial
            ));
        }
        self.memory64.push(memory.memory64);
        Ok(())
    }
}

struct Limits {
    shape: Shape,
    maximum_pages: u64,
    initial_pages: u64,
    next_function: usize,
    imports_added: bool,
}

impl Limits {
    fn fuel(&self) -> u32 {
        self.shape.imported_globals
    }

    fn pages(&self) -> u32 {
        self.shape.imported_globals + 1
    }

    fn add_imports(&mut self, imports: &mut ImportSection) {
        let global = EntityType::Global(GlobalType {
            val_type: ValType::I64,
            mutable: true,
            shared: false,
        });
        imports.import(LIMITS_MODULE, "fuel", global);
        imports.import(LIMITS_MODULE, "pages", global);
        self.imports_added = true;
    }

    /// Trap when fuel is exhausted, otherwise consume one unit. Stack-neutral.
    fn charge(&self, function: &mut Function) {
        let fuel = self.fuel();
        function
            .instruction(&Instruction::GlobalGet(fuel))
            .instruction(&Instruction::I64Eqz)
            .instruction(&Instruction::If(BlockType::Empty))
            .instruction(&Instruction::Unreachable)
            .instruction(&Instruction::End)
            .instruction(&Instruction::GlobalGet(fuel))
            .instruction(&Instruction::I64Const(1))
            .instruction(&Instruction::I64Sub)
            .instruction(&Instruction::GlobalSet(fuel));
    }

    /// `memory.grow` that fails (-1) unless the shared page budget covers the request.
    /// `scratch` is the first of the four appended locals: i32 delta, i32 result, i64 delta, i64 result.
    fn grow(&self, function: &mut Function, memory: u32, scratch: u32) -> Result<(), String> {
        let wide = *self
            .shape
            .memory64
            .get(memory as usize)
            .ok_or("memory.grow names an unknown memory")?;
        let (delta, result, ty, failed) = if wide {
            (
                scratch + 2,
                scratch + 3,
                ValType::I64,
                Instruction::I64Const(-1),
            )
        } else {
            (
                scratch,
                scratch + 1,
                ValType::I32,
                Instruction::I32Const(-1),
            )
        };
        let pages = self.pages();
        let widen = |function: &mut Function| {
            if !wide {
                function.instruction(&Instruction::I64ExtendI32U);
            }
        };
        function.instruction(&Instruction::LocalSet(delta));
        function.instruction(&Instruction::LocalGet(delta));
        widen(function);
        function
            .instruction(&Instruction::GlobalGet(pages))
            .instruction(&Instruction::I64GtU)
            .instruction(&Instruction::If(BlockType::Result(ty)))
            .instruction(&failed)
            .instruction(&Instruction::Else)
            .instruction(&Instruction::LocalGet(delta))
            .instruction(&Instruction::MemoryGrow(memory))
            .instruction(&Instruction::LocalTee(result))
            .instruction(&failed)
            .instruction(&if wide {
                Instruction::I64Ne
            } else {
                Instruction::I32Ne
            })
            .instruction(&Instruction::If(BlockType::Empty))
            .instruction(&Instruction::GlobalGet(pages))
            .instruction(&Instruction::LocalGet(delta));
        widen(function);
        function
            .instruction(&Instruction::I64Sub)
            .instruction(&Instruction::GlobalSet(pages))
            .instruction(&Instruction::End)
            .instruction(&Instruction::LocalGet(result))
            .instruction(&Instruction::End);
        Ok(())
    }
}

impl Reencode for Limits {
    type Error = String;

    /// Defined globals move behind the two appended imported globals.
    fn global_index(&mut self, global: u32) -> Result<u32, Error<String>> {
        if global < self.shape.imported_globals {
            return Ok(global);
        }
        global
            .checked_add(ADDED_GLOBALS)
            .ok_or_else(|| Error::UserError("too many globals".into()))
    }

    fn parse_import_section(
        &mut self,
        imports: &mut ImportSection,
        section: wasmparser::ImportSectionReader<'_>,
    ) -> Result<(), Error<String>> {
        utils::parse_import_section(self, imports, section)?;
        self.add_imports(imports);
        Ok(())
    }

    /// A module without imports receives an import section before its first later section.
    fn intersperse_section_hook(
        &mut self,
        module: &mut Module,
        after: Option<SectionId>,
        before: Option<SectionId>,
    ) -> Result<(), Error<String>> {
        utils::intersperse_section_hook(self, module, after, before)?;
        let later = !matches!(
            before,
            Some(SectionId::Custom | SectionId::Type | SectionId::Import)
        );
        if !self.shape.has_imports && !self.imports_added && later {
            let mut imports = ImportSection::new();
            self.add_imports(&mut imports);
            module.section(&imports);
        }
        Ok(())
    }

    /// Clamp each defined memory and count the pages one instantiation allocates.
    fn parse_memory_section(
        &mut self,
        memories: &mut MemorySection,
        section: wasmparser::MemorySectionReader<'_>,
    ) -> Result<(), Error<String>> {
        for memory in section {
            let memory = memory?;
            self.initial_pages = self
                .initial_pages
                .checked_add(memory.initial)
                .ok_or_else(|| Error::UserError("initial memory overflows".into()))?;
            let mut ty = utils::memory_type(self, memory);
            ty.maximum = Some(
                ty.maximum
                    .map_or(self.maximum_pages, |max| max.min(self.maximum_pages)),
            );
            memories.memory(ty);
        }
        Ok(())
    }

    fn parse_function_body(
        &mut self,
        code: &mut CodeSection,
        body: FunctionBody<'_>,
    ) -> Result<(), Error<String>> {
        let user = |message: &str| Error::UserError(message.to_owned());
        let ty = *self
            .shape
            .functions
            .get(self.next_function)
            .ok_or_else(|| user("function body without a declared type"))?;
        self.next_function += 1;
        let mut scratch = self
            .shape
            .params
            .get(ty as usize)
            .copied()
            .flatten()
            .ok_or_else(|| user("function body with a non-function type"))?;
        let mut locals = Vec::new();
        for local in body.get_locals_reader()? {
            let (count, ty) = local?;
            scratch = scratch
                .checked_add(count)
                .ok_or_else(|| user("too many locals"))?;
            locals.push((count, self.val_type(ty)?));
        }
        scratch
            .checked_add(4)
            .ok_or_else(|| user("too many locals"))?;
        // Appended after every original local, so no local index moves.
        locals.extend([(2, ValType::I32), (2, ValType::I64)]);
        let mut function = Function::new(locals);
        self.charge(&mut function);
        let mut operators = body.get_operators_reader()?;
        while !operators.eof() {
            match operators.read()? {
                Operator::Loop { blockty } => {
                    function.instruction(&Instruction::Loop(self.block_type(blockty)?));
                    self.charge(&mut function);
                }
                Operator::MemoryGrow { mem } => {
                    self.grow(&mut function, mem, scratch)
                        .map_err(Error::UserError)?;
                }
                operator => {
                    function.instruction(&self.instruction(operator)?);
                }
            }
        }
        code.function(&function);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
