//! Failed boundary conversions reject one request and keep the callback transport usable.
use serde_json::{Value, json};
use std::{rc::Rc, time::Duration};
use tembra::{Host, Orb, Service, core::Requirement};
mod support;
use support::Execution;
const SDK: &str = include_str!("../../../packages/ts/sdk/dist/embedded.js");

struct RejectRequest;
impl tembra::codec::Codec for RejectRequest {
    fn id(&self) -> &str {
        "tembra-json/1"
    }
    fn serialize(&self, value: &Value) -> tembra::Result<Vec<u8>> {
        if value["kind"] == "invoke" && value["input"][0] == "encode" {
            return Err(tembra::Error::invalid(
                "test_encode",
                "request encoding refused",
            ));
        }
        tembra::codec::encode(value)
    }
    fn deserialize(&self, bytes: &[u8]) -> tembra::Result<Value> {
        tembra::codec::decode(bytes)
    }
}

#[test]
fn codec_failures_in_requests_and_replies_do_not_stop_the_pump() {
    for cooperative in [false, true] {
        let tokio = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio::task::LocalSet::new().block_on(&tokio, async {
            let execution = Execution::new(cooperative);
            let native = Host::new(execution.clone());
            let mut runtime = execution.with_codec(native.clone(), SDK, Rc::new(RejectRequest), r#"({
                id: Tembra.jsonCodec.id,
                serialize(value) {
                    if(value?.value === 'reply') throw {code:'invalid',reason:'test_reply',message:'reply encoding refused'};
                    return Tembra.jsonCodec.serialize(value);
                },
                deserialize(bytes) {
                    const value = Tembra.jsonCodec.deserialize(bytes);
                    if(value?.kind === 'invoke' && value.input?.[0] === 'decode')
                        throw {code:'invalid',reason:'test_decode',message:'request decoding refused'};
                    return value;
                }
            })"#).unwrap();
            runtime.evaluate("host.install({name:'js',apiVersion:1,services:{js:{methods:{read:(_e,_c,input)=>input}}}})").await.unwrap();
            native.install(vec![Orb::new("native",1)
                .requires(Requirement { orb:"js".into(),api_version:1,methods:[("js".into(),["read".into()].into())].into(),hooks:Default::default() })
                .service(Service::new("native",|declaration|declaration.get("js",vec!["main".into()]))
                    .method("read",|dependency,ctx,(input,):(String,)|async move {
                        dependency.call_portable::<_,String>(&ctx,"read",(input,)).await
                    }))]).unwrap();
            let result = tokio::time::timeout(Duration::from_secs(1), runtime.evaluate("(async()=>{const r=await host.get('native','main');try{const results=[];for(const input of ['encode','decode','reply']){try{await r.read(input);results.push('allowed')}catch(e){results.push(e.reason)}results.push(await r.read('good'))}return results}finally{r.release()}})()")).await.unwrap().unwrap();
            assert_eq!(result,json!(["test_encode","good","test_decode","good","test_reply","good"]));
            tokio::time::timeout(Duration::from_secs(1),runtime.close()).await.unwrap().unwrap();
            assert_eq!(native.kernel().diagnostics().calls,0);
        });
    }
}

#[test]
fn native_callback_panics_reject_one_call_and_preserve_following_calls() {
    for cooperative in [false, true] {
        let tokio = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio::task::LocalSet::new().block_on(&tokio, async {
            let execution = Execution::new(cooperative);
            let native = Host::new(execution.clone());
            native.install(vec![Orb::new("native",1).service(Service::new("native",|_|Ok(()))
                .method("panic",|_,_,_:Vec<Value>|async {
                    panic!("native callback panic");
                    #[allow(unreachable_code)] Ok(())
                })
                .method("read",|_,_,_:Vec<Value>|async {Ok(42)}))]).unwrap();
            let mut runtime = execution.runtime(native.clone(),SDK).unwrap();
            let result = runtime.evaluate("(async()=>{const r=await host.get('native','main');try{let failure;try{await r.panic()}catch(e){failure=[e.code,e.reason]}return [failure,await r.read()]}finally{r.release()}})()").await.unwrap();
            assert_eq!(result,json!([["unavailable","callback"],42]));
            assert_eq!(native.kernel().diagnostics().calls,0);
            runtime.close().await.unwrap();
        });
    }
}

#[test]
fn embedded_monotonic_time_retains_submillisecond_precision() {
    let tokio = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    tokio::task::LocalSet::new().block_on(&tokio, async {
        let execution = Execution::new(false);
        let mut runtime = execution.runtime(Host::new(execution.clone()),SDK).unwrap();
        let result = runtime.evaluate("(()=>{for(let i=0;i<10000;i++){const value=performance.now();if(value%1!==0)return true}return false})()").await.unwrap();
        assert_eq!(result,json!(true));
        runtime.close().await.unwrap();
    });
}

struct RejectActivation;
impl tembra::codec::Codec for RejectActivation {
    fn id(&self) -> &str {
        "tembra-json/1"
    }
    fn serialize(&self, value: &Value) -> tembra::Result<Vec<u8>> {
        if value["kind"] == "activate" {
            return Err(tembra::Error::invalid(
                "activation_encode",
                "activation encoding refused",
            ));
        }
        tembra::codec::encode(value)
    }
    fn deserialize(&self, bytes: &[u8]) -> tembra::Result<Value> {
        tembra::codec::decode(bytes)
    }
}

#[test]
fn rejected_activation_transport_retires_reservations_and_wakes_dependents() {
    for cooperative in [false, true] {
        for reject_encode in [false, true] {
            let tokio = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            tokio::task::LocalSet::new().block_on(&tokio, async {
                let execution = Execution::new(cooperative);
                let native = Host::new(execution.clone());
                let codec: Rc<dyn tembra::codec::Codec> = if reject_encode { Rc::new(RejectActivation) } else { Rc::new(tembra::codec::Json) };
                let js_codec = if reject_encode { "Tembra.jsonCodec" } else { "({id:Tembra.jsonCodec.id,serialize:Tembra.jsonCodec.serialize,deserialize(bytes){const value=Tembra.jsonCodec.deserialize(bytes);if(value?.kind==='activate')throw Error('activation decoding refused');return value}})" };
                let mut runtime = execution.with_codec(native.clone(),SDK,codec,js_codec).unwrap();
                runtime.evaluate("host.install({name:'js',apiVersion:1,services:{js:{methods:{read:()=>42}}}})").await.unwrap();
                native.install(vec![Orb::new("native",1)
                    .requires(Requirement{orb:"js".into(),api_version:1,methods:[("js".into(),["read".into()].into())].into(),hooks:Default::default()})
                    .service(Service::new("native",|d|d.get("js",vec!["main".into()]))
                        .method("read",|r,c,_:Vec<Value>|async move{r.call_portable::<_,u32>(&c,"read",Vec::<Value>::new()).await}))]).unwrap();
                let result = tokio::time::timeout(Duration::from_secs(1),runtime.evaluate("(async()=>{let ref;try{ref=await host.get('native','main');await ref.read();return 'allowed'}catch{return 'rejected'}finally{ref?.release()}})()")).await.expect("failed activation stranded its dependent").unwrap();
                assert_eq!(result,json!("rejected"));
                assert_eq!(native.kernel().diagnostics().objects,0);
                tokio::time::timeout(Duration::from_secs(1),runtime.close()).await.unwrap().unwrap();
            });
        }
    }
}
