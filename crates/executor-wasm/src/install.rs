//! Installation chooses dependency providers and permissions; artifacts carry neither authority.
use crate::Grants;
use serde::Deserialize;
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};
use tembra_contracts::OrbProvider;

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Install {
    pub(crate) requires: BTreeMap<String, OrbProvider>,
    pub grants: Grants,
    /// Standard CLI data of this Orb; empty by default, never inherited from the host process.
    pub cli: Cli,
}

/// A fresh byte source for each Store of an Orb (Rust embedders only).
pub type ReaderFactory =
    Arc<dyn Fn() -> Box<dyn tokio::io::AsyncRead + Send + Sync + Unpin> + Send + Sync>;
/// A fresh byte sink for each Store of an Orb (Rust embedders only).
pub type WriterFactory =
    Arc<dyn Fn() -> Box<dyn tokio::io::AsyncWrite + Send + Sync + Unpin> + Send + Sync>;

/// `wasi:cli` configuration. Reached only through explicit imports and the CLI system grant.
#[derive(Clone, Default, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Cli {
    pub arguments: Vec<String>,
    pub environment: Vec<(String, String)>,
    pub initial_cwd: Option<String>,
    pub stdin: Input,
    pub stdout: Output,
    pub stderr: Output,
}

/// Standard input bytes; each Store reads from the start.
#[derive(Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub enum Input {
    /// Immediately at end of stream.
    #[default]
    Empty,
    /// These bytes, copied once at installation.
    Bytes(Vec<u8>),
    /// A regular host file opened for each Store; devices, terminals and FIFOs are refused.
    File(PathBuf),
    /// A Rust byte source factory.
    #[serde(skip)]
    Reader(ReaderFactory),
}

/// Standard output or error bytes, written unchanged.
#[derive(Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub enum Output {
    /// Accepted and discarded.
    #[default]
    Discard,
    /// Appended to a regular host file (created if absent) opened for each Store.
    File(PathBuf),
    /// A Rust byte sink factory.
    #[serde(skip)]
    Writer(WriterFactory),
}

impl Install {
    /// Select one installed Orb for a WIT import; WIT supplies its methods and signatures.
    pub fn requires(
        mut self,
        import: impl Into<String>,
        orb: impl Into<String>,
        api_version: u32,
    ) -> Self {
        self.requires.insert(
            import.into(),
            OrbProvider {
                orb: orb.into(),
                api_version,
            },
        );
        self
    }

    /// Configure this Orb's standard CLI data and streams.
    pub fn cli(mut self, cli: Cli) -> Self {
        self.cli = cli;
        self
    }
}
