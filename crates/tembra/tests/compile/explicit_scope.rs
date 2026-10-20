#[tembra::contract(local, name = "compile.explicit-local", version = 1)]
trait Local {
    async fn echo(&self, value: std::rc::Rc<std::cell::Cell<u64>>)
        -> tembra::Result<std::rc::Rc<std::cell::Cell<u64>>>;
}

struct LocalService;
impl LocalProvider for LocalService {
    async fn echo(
        &self,
        value: std::rc::Rc<std::cell::Cell<u64>>,
        _ctx: tembra::Context,
    ) -> tembra::Result<std::rc::Rc<std::cell::Cell<u64>>> {
        Ok(value)
    }
}

#[tembra::contract(remote, name = "compile.explicit-remote", version = 1, host = "service")]
trait Remote {
    async fn echo(&self, value: Option<u64>) -> tembra::Result<Option<u64>>;
}

struct RemoteService;
impl RemoteProvider for RemoteService {
    async fn echo(&self, value: Option<u64>, _ctx: tembra::Context)
        -> tembra::Result<Option<u64>> {
        Ok(value)
    }
}
