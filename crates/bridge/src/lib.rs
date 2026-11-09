//! Metadata-only kernel protocol. User arguments, state values and callbacks never cross it; only
//! lifecycle metadata and opaque host-issued identities do.
//!
//! The command table in `commands` is the single definition of every operation. Rust callers use
//! [`Kernel::run`] with a typed [`Command`]; platform adapters decode Node-API or wasm-bindgen objects through [`Kernel::dispatch`]; string-only platforms use [`Kernel::request`].

#[macro_use]
pub mod protocol;
#[macro_use]
pub mod decode;
#[macro_use]
pub mod wire;
mod commands;
mod typescript;

pub use commands::{Command, Reply, input};
pub use protocol::KernelOp;
pub use tembra_core as core;
pub use typescript::typescript;

use serde::{Deserializer, Serialize};
use tembra_core::{Error, Host, Reason, Result};

#[derive(Clone, Default)]
pub struct Kernel {
    pub host: Host,
}

/// A structured result envelope: `{ ok: result }` or `{ error: { code, reason, message } }`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Ok(Reply),
    Error(wire::KernelError),
}

impl From<Result<Reply>> for Outcome {
    fn from(result: Result<Reply>) -> Self {
        match result {
            Ok(reply) => Self::Ok(reply),
            Err(error) => Self::Error(error.into()),
        }
    }
}

impl Kernel {
    /// Create a metadata bridge with a fresh kernel host.
    pub fn new() -> Self {
        Self::default()
    }

    /// Execute one typed command directly against the core.
    pub fn run<C: Command>(&self, command: C) -> Result<C::Output> {
        command.run(self)
    }

    /// Scalar hot entry: success carries no metadata and never builds an input tree.
    /// Failures retain the same structured error shape as the command path.
    pub fn check(&self, root: f64, call: Option<f64>) -> Option<String> {
        let result = (|| {
            self.run(input::CheckContext {
                root: wire::Handle::number(root)?,
                call: call.map(wire::Handle::number).transpose()?,
            })
        })();
        result.err().map(|error| {
            serde_json::to_string(&wire::KernelError::from(error))
                .expect("serializable kernel error")
        })
    }

    /// Decode one named command from a structured platform value and execute it.
    ///
    /// The deserializer must expose native scalar values, as Node-API and wasm-bindgen do.
    /// For Rust data use `run`; JSON text uses `request`, which preserves JSON numeric syntax
    /// independently of serde_json's optional private numeric representation.
    ///
    /// Unknown and disabled operations execute nothing. Inputs are decoded completely, within the
    /// metadata budget and with unknown fields rejected, before the core is called.
    pub fn dispatch<'de, D: Deserializer<'de>>(&self, op: &str, input: D) -> Result<Reply> {
        let op = KernelOp::parse(op)
            .filter(|op| op.enabled())
            .ok_or_else(|| commands::unavailable(op))?;
        self.decode_run(op, decode::read(input)?)
    }

    /// Decode, execute and wrap one command in its structured envelope.
    pub fn outcome<'de, D: Deserializer<'de>>(&self, op: &str, input: D) -> Outcome {
        self.dispatch(op, input).into()
    }

    /// Exchange one JSON request, `{ "op": name, ...fields }`, for a JSON envelope.
    ///
    /// This is the representation for string-only platforms. Malformed or oversized input executes
    /// no command.
    pub fn request(&self, request: &str) -> String {
        let outcome = Outcome::from(self.request_json(request));
        serde_json::to_string(&outcome).unwrap_or_else(|error| {
            let error = Error::invalid(Reason::Serialization, error.to_string());
            serde_json::to_string(&Outcome::Error(error.into()))
                .unwrap_or_else(|_| String::from("{\"error\":null}"))
        })
    }

    /// Decode one JSON request object and dispatch its named command.
    fn request_json(&self, request: &str) -> Result<Reply> {
        if request.len() > decode::MAX_UNITS {
            return Err(Error::invalid(
                Reason::MetadataSize,
                "metadata exceeds one mebibyte",
            ));
        }
        let input = decode::read_json(request)?;
        let mut fields = decode::Fields::new(input)?;
        let name = fields.take_op()?;
        let op = KernelOp::parse(&name)
            .filter(|op| op.enabled())
            .ok_or_else(|| commands::unavailable(&name))?;
        self.decode_run(op, fields.into_input())
    }
}
