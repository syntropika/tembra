//! Run the same public integration cases through both execution paths.
use futures::task::{LocalFutureObj, LocalSpawn, SpawnError};
use std::rc::Rc;
use tembra::{Host, Result};
use tembra_executor_js::{CooperativeExecutor, Runtime, TokioExecutor};

#[derive(Clone)]
pub enum Execution {
    Standard(TokioExecutor),
    Cooperative(CooperativeExecutor),
}
impl Execution {
    pub fn new(cooperative: bool) -> Self {
        if cooperative {
            Self::Cooperative(CooperativeExecutor::new())
        } else {
            Self::Standard(TokioExecutor::new())
        }
    }
    pub fn runtime(&self, native: Host, sdk: impl Into<String>) -> Result<Runtime> {
        self.with_codec(
            native,
            sdk,
            Rc::new(tembra::codec::Json),
            "Tembra.jsonCodec",
        )
    }
    pub fn with_codec(
        &self,
        native: Host,
        sdk: impl Into<String>,
        codec: Rc<dyn tembra::codec::Codec>,
        js_codec: &str,
    ) -> Result<Runtime> {
        match self {
            Self::Standard(_) => Runtime::with_codec(native, sdk, codec, js_codec),
            Self::Cooperative(driver) => {
                Runtime::with_codec_and_driver(native, sdk, codec, js_codec, driver.clone())
            }
        }
    }
}
impl LocalSpawn for Execution {
    fn spawn_local_obj(
        &self,
        future: LocalFutureObj<'static, ()>,
    ) -> std::result::Result<(), SpawnError> {
        match self {
            Self::Standard(executor) => executor.spawn_local_obj(future),
            Self::Cooperative(executor) => executor.spawn_local_obj(future),
        }
    }
}
