//! Build-time preparation of compiled components for browser execution.
//!
//! Browsers receive no WebAssembly parser, encoder or bindings generator. Tembra's tooling uses
//! these functions after the native verifier accepted an artifact: it reads the component's exact
//! top-level import names and instruments every generated core module with fuel and page limits.
use napi_derive::napi;
use wasmparser::{Encoding, Parser, Payload};

mod limits;

/// Pages of a 32-bit linear memory: the largest per-installation budget a browser can grant.
pub const MAXIMUM_PAGES: u32 = 1 << 16;

/// One instrumented core module and the pages each of its instantiations allocates.
#[napi(object)]
pub struct BrowserCore {
    pub bytes: napi::bindgen_prelude::Buffer,
    pub initial_pages: u32,
}

/// Instrument one generated core module with the shared `tembra:limits` fuel and page globals.
///
/// `maximum_pages` clamps every memory the module defines; the runtime budget is the global.
#[napi]
pub fn instrument_core(
    bytes: napi::bindgen_prelude::Uint8Array,
    maximum_pages: u32,
) -> napi::Result<BrowserCore> {
    if maximum_pages == 0 || maximum_pages > MAXIMUM_PAGES {
        return Err(napi::Error::from_reason(format!(
            "maximum pages must be between 1 and {MAXIMUM_PAGES}"
        )));
    }
    let core =
        limits::instrument(&bytes, maximum_pages.into()).map_err(napi::Error::from_reason)?;
    Ok(BrowserCore {
        initial_pages: u32::try_from(core.initial_pages)
            .map_err(|_| napi::Error::from_reason("initial memory exceeds the page limit"))?,
        bytes: core.bytes.into(),
    })
}

/// The exact, versioned top-level import names of a component, in binary order.
#[napi]
pub fn component_imports(bytes: napi::bindgen_prelude::Uint8Array) -> napi::Result<Vec<String>> {
    imports(&bytes).map_err(napi::Error::from_reason)
}

fn imports(bytes: &[u8]) -> Result<Vec<String>, String> {
    let mut names = Vec::new();
    // Nested modules and components open with their own header and close with `End`.
    let mut depth = 0usize;
    for payload in Parser::new(0).parse_all(bytes) {
        match payload.map_err(|error| error.to_string())? {
            Payload::Version { encoding, .. } => {
                if depth == 0 && encoding != Encoding::Component {
                    return Err("expected a WebAssembly component".into());
                }
                depth += 1;
            }
            Payload::End(_) => depth = depth.saturating_sub(1),
            Payload::ComponentImportSection(reader) if depth == 1 => {
                for import in reader {
                    let import = import.map_err(|error| error.to_string())?;
                    if import.name.implements.is_some() {
                        return Err("browser components do not support `implements` imports".into());
                    }
                    names.push(import.name.full_name().into_owned());
                }
            }
            _ => {}
        }
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::imports;

    #[test]
    fn only_top_level_component_imports_are_listed_with_exact_versions() {
        let component = wat::parse_str(
            r#"(component
                (import "wasi:clocks/monotonic-clock@0.3.0" (instance))
                (component (import "nested:skip/me@1.0.0" (instance)))
                (import "example:math/ops@1.2.0" (instance))
            )"#,
        )
        .unwrap();
        assert_eq!(
            imports(&component).unwrap(),
            [
                "wasi:clocks/monotonic-clock@0.3.0",
                "example:math/ops@1.2.0"
            ]
        );
        let module = wat::parse_str("(module)").unwrap();
        assert!(imports(&module).is_err());
    }
}
