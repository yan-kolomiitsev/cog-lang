use cog::sdk::{Engine, Signature, Type, Value};

#[test]
fn registered_callbacks_round_trip_and_outlive_the_engine() {
    let mut engine = Engine::new();
    engine
        .register(
            "host::add",
            Signature::new(vec![Type::Int, Type::Int], Type::Int),
            |_, arguments| Ok(Value::int(arguments[0].as_int()? + arguments[1].as_int()?)),
        )
        .unwrap();
    let program = engine
        .compile(
            "sdk.cog",
            "include std; use io; use host; fn main() { io::println(host::add(20, 22)); }",
        )
        .unwrap();
    let encoded = program.encode().unwrap();
    let program = engine.decode(&encoded).unwrap();
    drop(engine);
    let mut output = Vec::new();
    program.run(&mut output).unwrap();
    assert_eq!(output, b"42\n");
}

#[test]
fn registrations_are_engine_scoped_and_check_typed_arguments() {
    let mut engine = Engine::new();
    engine
        .register(
            "host::identity",
            Signature::new(vec![Type::Int], Type::Int),
            |_, arguments| Ok(arguments[0].clone()),
        )
        .unwrap();
    let source = "include std; use host; fn main() { host::identity(1); }";
    assert!(engine.compile("sdk.cog", source).is_ok());
    assert!(Engine::new().compile("sdk.cog", source).is_err());
    assert!(
        engine
            .compile(
                "sdk.cog",
                "include std; use host; fn main() { host::identity(\"bad\"); }"
            )
            .is_err()
    );
}

#[test]
fn callback_panics_become_catchable_native_failures() {
    let mut engine = Engine::new();
    engine
        .register(
            "host::fail",
            Signature::new(vec![], Type::Nil),
            |_, _| -> Result<Value, cog::sdk::Error> {
                panic!("test panic");
            },
        )
        .unwrap();
    let program = engine.compile("panic.cog", "include std; use io; use host; use err; fn main() { try { host::fail(); } catch(e) { io::println(e.error_kind()); } }").unwrap();
    let mut output = Vec::new();
    program.run(&mut output).unwrap();
    assert_eq!(output, b"NativeFailure\n");
}

#[test]
fn metadata_cannot_execute_and_contract_changes_cannot_link() {
    use cog::sdk::Effects;
    let source = "include std; use io; use host; fn main() { io::println(1); host::work(); }";
    let signature = Signature::new(vec![], Type::Nil);
    let mut compiler = Engine::new();
    compiler
        .register_metadata("host::work", signature.clone())
        .unwrap();
    let program = compiler.compile("metadata.cog", source).unwrap();
    let bytes = program.encode().unwrap();
    let mut output = Vec::new();
    assert!(
        program
            .run(&mut output)
            .unwrap_err()
            .message()
            .contains("not linked")
    );
    assert!(
        output.is_empty(),
        "linking must fail before executing any instruction"
    );
    let mut changed = Engine::new();
    changed
        .register(
            "host::work",
            signature.clone().with_effects(Effects::OUTPUT),
            |_, _| Ok(Value::nil()),
        )
        .unwrap();
    assert!(changed.decode(&bytes).is_err());
    assert!(Engine::new().decode(&bytes).is_err());
    let mut linked = Engine::new();
    linked
        .register("host::work", signature, |_, _| Ok(Value::nil()))
        .unwrap();
    linked.decode(&bytes).unwrap().run(&mut output).unwrap();
    assert_eq!(output, b"1\n");
}

#[test]
fn wrong_results_and_callback_errors_follow_the_runtime_error_path() {
    let mut engine = Engine::new();
    engine
        .register("host::wrong", Signature::new(vec![], Type::Int), |_, _| {
            Ok(Value::string("bad"))
        })
        .unwrap();
    engine
        .register("host::fail", Signature::new(vec![], Type::Nil), |_, _| {
            Err(cog::sdk::Error::new("failed"))
        })
        .unwrap();
    let source = "include std; use io; use host; use err; fn main() { try { host::wrong(); } catch(e) { io::println(e.error_kind()); } try { host::fail(); } catch(e) { io::println(e.error_kind()); } }";
    let program = engine
        .decode(
            &engine
                .compile("errors.cog", source)
                .unwrap()
                .encode()
                .unwrap(),
        )
        .unwrap();
    let mut output = Vec::new();
    program.run(&mut output).unwrap();
    assert_eq!(output, b"NativeFailure\nNativeFailure\n");
}

#[test]
fn optional_effect_policy_checks_before_invoking_callbacks() {
    use cog::sdk::{Effects, Permissions, RunContext};
    use std::cell::Cell;
    use std::rc::Rc;
    let calls = Rc::new(Cell::new(0));
    let retained = calls.clone();
    let mut engine = Engine::new();
    engine
        .register(
            "host::write",
            Signature::new(vec![], Type::Nil).with_effects(Effects::OUTPUT),
            move |context, _| {
                retained.set(retained.get() + 1);
                context.write_output("called\n")?;
                Ok(Value::nil())
            },
        )
        .unwrap();
    let program = engine
        .compile(
            "effects.cog",
            "include std; use host; fn main() { host::write()!!; }",
        )
        .unwrap();
    let mut output = Vec::new();
    assert!(
        program
            .run_in(&mut RunContext::new(&mut output).with_permissions(Permissions::none()))
            .is_err()
    );
    assert_eq!(calls.get(), 0);
    program.run(&mut output).unwrap();
    assert_eq!(calls.get(), 1);
    assert_eq!(output, b"called\n");
}

#[test]
fn reentry_is_rejected_and_callback_arguments_are_owned_roots() {
    use std::cell::RefCell;
    use std::rc::Rc;
    let nested = Rc::new(Engine::new().compile("nested.cog", "fn main() {}").unwrap());
    let saved = Rc::new(RefCell::new(None));
    let retained = saved.clone();
    let mut engine = Engine::new();
    engine
        .register(
            "host::retain",
            Signature::new(vec![Type::Array], Type::Nil),
            move |_, values| {
                assert_eq!(
                    nested.run(&mut Vec::new()).unwrap_err().kind(),
                    cog::sdk::ErrorKind::Reentrant
                );
                *retained.borrow_mut() = Some(values[0].clone());
                Ok(Value::nil())
            },
        )
        .unwrap();
    engine
        .compile(
            "roots.cog",
            "include std; use host; fn main() { host::retain([42]); }",
        )
        .unwrap()
        .run(&mut Vec::new())
        .unwrap();
    drop(engine);
    assert_eq!(
        saved
            .borrow()
            .as_ref()
            .unwrap()
            .array_get(0)
            .unwrap()
            .as_int()
            .unwrap(),
        42
    );
    Engine::new()
        .compile("after.cog", "fn main() {}")
        .unwrap()
        .run(&mut Vec::new())
        .unwrap();
}

#[test]
fn invalid_registration_is_rejected_without_losing_prior_entries() {
    let mut engine = Engine::new();
    engine
        .register("host::keep", Signature::new(vec![], Type::Nil), |_, _| {
            Ok(Value::nil())
        })
        .unwrap();
    for name in [
        "host::keep",
        "io::print",
        "err::custom",
        "host",
        "host::deep::name",
        "host::bad\n",
    ] {
        assert!(
            engine
                .register_metadata(name, Signature::new(vec![], Type::Nil))
                .is_err(),
            "{name:?}"
        );
    }
    assert!(
        engine
            .register_metadata(
                "host::bad",
                Signature::new(vec![Type::Array], Type::Nil).with_mutating_parameters(vec![1])
            )
            .is_err()
    );
    engine
        .compile(
            "after.cog",
            "include std; use host; fn main() { host::keep(); }",
        )
        .unwrap()
        .run(&mut Vec::new())
        .unwrap();
}

#[test]
fn typed_native_parameters_propagate_errors_and_any_can_handle_them() {
    let mut engine = Engine::new();
    engine
        .register(
            "host::typed",
            Signature::new(vec![Type::Int], Type::Int),
            |_, _| panic!("typed Error must bypass the callback"),
        )
        .unwrap();
    engine
        .register(
            "host::dynamic",
            Signature::new(vec![Type::Any], Type::Bool),
            |_, arguments| Ok(Value::boolean(arguments[0].is_error())),
        )
        .unwrap();
    let source = "include std; use host; use io; use err; fn main() { try { host::typed(1 / 0); } catch(e) { io::println(e.error_kind()); } io::println(host::dynamic(1 / 0)); }";
    let program = engine
        .decode(
            &engine
                .compile("error-arguments.cog", source)
                .unwrap()
                .encode()
                .unwrap(),
        )
        .unwrap();
    let mut output = Vec::new();
    program.run(&mut output).unwrap();
    assert_eq!(output, b"DivisionByZero\ntrue\n");
}

#[test]
fn mutation_contracts_check_source_aliases_and_portable_identity() {
    let signature = Signature::new(vec![Type::Array], Type::Nil).with_mutating_parameters(vec![0]);
    let mut engine = Engine::new();
    engine
        .register("host::change", signature, |_, arguments| {
            arguments[0].array_set(0, Value::int(42))?;
            Ok(Value::nil())
        })
        .unwrap();
    assert!(
        engine
            .compile(
                "const.cog",
                "include std; use host; fn main() { const a = [1]; host::change(a); }"
            )
            .is_err()
    );
    let source = "include std; use host; use io; fn main() { var a = [1]; host::change(a); io::println(a); }";
    let encoded = engine
        .compile("mutate.cog", source)
        .unwrap()
        .encode()
        .unwrap();
    let mut output = Vec::new();
    engine.decode(&encoded).unwrap().run(&mut output).unwrap();
    assert_eq!(output, b"[42]\n");
    let mut changed = Engine::new();
    changed
        .register(
            "host::change",
            Signature::new(vec![Type::Array], Type::Nil),
            |_, _| Ok(Value::nil()),
        )
        .unwrap();
    assert!(changed.decode(&encoded).is_err());
}

#[test]
fn run_context_applies_optional_buffer_limits() {
    use cog::sdk::{Limits, RunContext};

    let program = Engine::new()
        .compile(
            "limits.cog",
            "include std; use io; use bytes; fn main() { try { bytes::new(3)!!; io::println(\"ok\"); } catch(error) { io::println(\"R\"); } }",
        )
        .unwrap();

    let mut output = Vec::new();
    program.run(&mut output).unwrap();
    assert_eq!(output, b"ok\n");

    let mut output = Vec::new();
    let limits = Limits {
        max_buffer_bytes: Some(2),
        ..Limits::default()
    };
    let error = program
        .run_in(&mut RunContext::new(&mut output).with_limits(limits))
        .unwrap_err();
    assert_eq!(error.kind(), cog::sdk::ErrorKind::Runtime);
    assert!(error.message().contains("runtime resource limit: bytes"));
    assert!(output.is_empty(), "the failing allocation precedes output");
}
