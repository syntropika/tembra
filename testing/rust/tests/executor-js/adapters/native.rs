use super::*;

struct Marker;
struct Other;
struct MarkerCodec;
impl ScopedCodec<Rc<Marker>> for MarkerCodec {
    fn encode(&self, value: Rc<Marker>) -> Result<NativeValue> {
        Ok(NativeValue::capability(value))
    }
    fn decode(&self, value: NativeValue) -> Result<Rc<Marker>> {
        value.downcast()
    }
}
struct OtherCodec;
impl ScopedCodec<Rc<Other>> for OtherCodec {
    fn encode(&self, value: Rc<Other>) -> Result<NativeValue> {
        Ok(NativeValue::capability(value))
    }
    fn decode(&self, value: NativeValue) -> Result<Rc<Other>> {
        value.downcast()
    }
}

#[test]
fn shared_javascript_function_has_distinct_aliases_for_different_native_signatures() {
    run(|driver| distinct_aliases(driver).boxed_local());
}

async fn distinct_aliases(driver: Rc<TokioDriver>) {
    let host = Host::new(
        HostOptions::new("alias-identity", json!(null)),
        driver.clone(),
    );
    let executor = Executor::new(driver, ExecutorOptions::default()).unwrap();
    for name in ["marker@1", "other@1"] {
        let signature = NativeMethodSignature::new(Some(name.into()), Some(name.into())).unwrap();
        if name == "marker@1" {
            executor
                .register_native_method(
                    signature,
                    NativeMethodCodec::new(Rc::new(MarkerCodec), Rc::new(MarkerCodec)),
                )
                .unwrap();
        } else {
            executor
                .register_native_method(
                    signature,
                    NativeMethodCodec::new(Rc::new(OtherCodec), Rc::new(OtherCodec)),
                )
                .unwrap();
        }
    }
    let method = |id: &str| Member::Method {
        args: Some(ValueSchema::Native(id.into())),
        result: Some(ValueSchema::Native(id.into())),
    };
    let provider = token(
        "shared-function",
        2,
        BTreeMap::from([
            ("a".into(), method("marker@1")),
            ("b".into(), method("other@1")),
        ]),
    );
    let consumer = token(
        "shared-function",
        1,
        BTreeMap::from([("old".into(), method("other@1"))]),
    );
    let source = format!(
        r#"
const provider={provider};
export default{{name:'adapters',uses:[],setup(scope,declare){{
    declare.serve(provider,()=>{{
        const shared=async(ctx,value)=>value;
        return{{methods:{{a:shared,b:shared}}}};
    }},{{accepts:[{adapter}]}});
}}}};
"#,
        provider = descriptor(&provider),
        adapter = adapter(&provider, &consumer, "{methods:{old:current.methods.b}}")
    );
    host.install_executable(
        prepare(&host, &executor, source).await,
        LoadOptions::default(),
    )
    .unwrap()
    .await
    .unwrap();
    let reference = host
        .get(consumer, json!({}), GetOptions::default())
        .await
        .unwrap();
    let expected = Rc::new(Other);
    let actual = reference
        .call::<Rc<Other>, Rc<Other>>("old", expected.clone(), None)
        .await
        .unwrap();
    assert!(Rc::ptr_eq(&actual, &expected));
    reference.release();
    host.close().await.unwrap();
    assert_closed(&executor);
}

#[test]
fn native_alias_preserves_actual_rc_opaque_resource_and_the_registered_wrapper() {
    run(|driver| native_alias(driver, false).boxed_local());
}

#[test]
fn identity_adapter_hides_extra_methods_opaque_and_state_from_the_legacy_consumer() {
    run(|driver| native_alias(driver, true).boxed_local());
}

async fn native_alias(driver: Rc<TokioDriver>, identity: bool) {
    let host = Host::new(
        HostOptions::new("native-adapter", json!(null)),
        driver.clone(),
    );
    let executor = Executor::new(driver, ExecutorOptions::default()).unwrap();
    executor
        .register_native_opaque("marker@1", Rc::new(MarkerCodec))
        .unwrap();
    let wrappers = Rc::new(Cell::new(0));
    let wrapped = wrappers.clone();
    executor
        .register_native_method(
            NativeMethodSignature::new(Some("marker@1".into()), Some("marker@1".into())).unwrap(),
            NativeMethodCodec::new(Rc::new(MarkerCodec), Rc::new(MarkerCodec)).with_wrapper(
                Rc::new(move |value, context, handler| {
                    wrapped.set(wrapped.get() + 1);
                    handler(value, context)
                }),
            ),
        )
        .unwrap();
    let method = || Member::Method {
        args: Some(ValueSchema::Native("marker@1".into())),
        result: Some(ValueSchema::Native("marker@1".into())),
    };
    let opaque = || Member::Opaque {
        id: "marker@1".into(),
    };
    let mut provider_members =
        BTreeMap::from([("echo".into(), method()), ("handle".into(), opaque())]);
    if identity {
        provider_members.extend([
            (
                "extraMethod".into(),
                Member::Method {
                    args: None,
                    result: Some(portable(json!({"type":"integer"}))),
                },
            ),
            ("extraOpaque".into(), opaque()),
            (
                "extraState".into(),
                Member::State {
                    schema: json!({"type":"integer"}),
                },
            ),
        ]);
    }
    let provider = token("native-adapted", 2, provider_members);
    let (call_name, opaque_name) = if identity {
        ("echo", "handle")
    } else {
        ("bounce", "oldHandle")
    };
    let consumer = token(
        "native-adapted",
        1,
        BTreeMap::from([(call_name.into(), method()), (opaque_name.into(), opaque())]),
    );
    let dependency = token(
        "marker-owner",
        1,
        BTreeMap::from([("marker".into(), opaque())]),
    );
    let marker = Rc::new(Marker);
    let exported = marker.clone();
    host.serve(
        dependency.clone(),
        Rc::new(move |_, _, declare| {
            let value = exported.clone();
            let resource = declare.resource(
                "marker",
                ResourceDeclaration::new(|_| async { Ok(()) }.boxed_local()),
            )?;
            Ok(Implementation::new().map_opaque("marker", resource, move |_| Ok(value.clone())))
        }),
        ServeOptions::default(),
    )
    .unwrap();
    let source = format!(
        r#"
const provider={provider}, dependency={dependency};
export default {{name:'adapters',uses:[dependency],setup(scope,declare){{
    declare.serve(provider,(input,self)=>{{
        const reference=self.get(dependency,{{}});
        const resource=self.resource('marker',{{deps:[reference],open:()=>reference.marker}});
        {extra}
        return {{methods:{{echo:async(ctx,value)=>value{extra_method}}},
            opaque:{{handle:resource{extra_opaque}}}{extra_state}}};
    }},{{accepts:[{adapter}]}});
}}}};
"#,
        provider = descriptor(&provider),
        dependency = descriptor(&dependency),
        extra = if identity {
            "const extra=self.cell('extra',{schema:{type:'integer'},initial:4});"
        } else {
            ""
        },
        extra_method = if identity { ",extraMethod:()=>7" } else { "" },
        extra_opaque = if identity {
            ",extraOpaque:resource"
        } else {
            ""
        },
        extra_state = if identity {
            ",state:{extraState:extra}"
        } else {
            ""
        },
        adapter = adapter(
            &provider,
            &consumer,
            if identity {
                "current"
            } else {
                "{methods:{bounce:current.methods.echo},opaque:{oldHandle:current.opaque.handle}}"
            }
        ),
    );
    host.install_executable(
        prepare(&host, &executor, source).await,
        LoadOptions::default(),
    )
    .unwrap()
    .await
    .unwrap();
    let reference = host
        .get(consumer, json!({}), GetOptions::default())
        .await
        .unwrap();
    let returned = reference
        .call::<Rc<Marker>, Rc<Marker>>(call_name, marker.clone(), None)
        .await
        .unwrap();
    assert!(Rc::ptr_eq(&returned, &marker));
    assert_eq!(
        wrappers.get(),
        1,
        "tagged alias must preserve its original checked wrapper"
    );
    assert!(Rc::ptr_eq(
        &reference.opaque::<Marker>(opaque_name).unwrap(),
        &marker
    ));
    if identity {
        assert_eq!(
            reference
                .call_json("extraMethod", None, None)
                .await
                .unwrap_err()
                .code,
            ErrorCode::NotInVariant
        );
        assert_eq!(
            reference
                .opaque::<Marker>("extraOpaque")
                .err()
                .unwrap()
                .code,
            ErrorCode::NotInVariant
        );
        assert_eq!(
            reference.readable("extraState").err().unwrap().code,
            ErrorCode::NotInVariant
        );
    } else {
        assert_eq!(
            reference
                .call::<Rc<Marker>, Rc<Marker>>("echo", marker, None)
                .await
                .err()
                .unwrap()
                .code,
            ErrorCode::NotInVariant
        );
    }
    reference.release();
    host.close().await.unwrap();
    assert!(host.errors().is_empty(), "{:?}", host.errors());
    assert_closed(&executor);
}
