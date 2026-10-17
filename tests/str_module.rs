use cog::runtime::{RuntimeContext, VmCapabilities};
use cog::{bytecode, interpreter, project};
use std::io::Cursor;

fn compile(body: &str) -> cog::backend::CompiledProgram {
    let source = format!(
        r#"include std; use err::Error;
        fn require(ok: bool) -> nil {{ if !ok {{ crash("assertion failed"); }} }}
        {body}"#
    );
    let program = project::compile_source("str_module.cog", &source)
        .unwrap_or_else(|error| panic!("{error}"));
    bytecode::decode(&bytecode::encode(&program).unwrap()).unwrap()
}

fn literal(text: &str) -> String {
    let mut escaped = String::from("\"");
    for ch in text.chars() {
        escaped.push_str(&match ch {
            '\\' => "\\\\".into(),
            '"' => "\\\"".into(),
            '\n' => "\\n".into(),
            '\r' => "\\r".into(),
            '\t' => "\\t".into(),
            '\0' => "\\0".into(),
            other => other.to_string(),
        });
    }
    escaped.push('"');
    escaped
}

#[test]
fn trim_handles_unicode_whitespace_and_preserves_other_characters() {
    let whitespace = [
        '\u{0009}', '\u{000a}', '\u{000b}', '\u{000c}', '\u{000d}', '\u{0020}', '\u{0085}',
        '\u{00a0}', '\u{1680}', '\u{2000}', '\u{2001}', '\u{2002}', '\u{2003}', '\u{2004}',
        '\u{2005}', '\u{2006}', '\u{2007}', '\u{2008}', '\u{2009}', '\u{200a}', '\u{2028}',
        '\u{2029}', '\u{202f}', '\u{205f}', '\u{3000}',
    ];
    assert!(whitespace.into_iter().all(char::is_whitespace));
    let mut cases = vec![
        String::new(),
        "plain".into(),
        "  α🦀 Cog \t inside  ".into(),
        whitespace.iter().collect::<String>(),
        "\0 x \0".into(),
        "\u{001c}\u{001d}\u{001e}\u{001f}".into(),
        "\u{feff} 42 \u{feff}".into(),
        "\u{200b} 42 \u{200b}".into(),
    ];
    for ch in whitespace {
        cases.push(format!("{ch}α🦀{ch} x{ch}"));
        cases.push(ch.to_string());
    }
    // Neighbours of whitespace codepoints must not be trimmed accidentally.
    for ch in [
        '\u{0084}', '\u{0086}', '\u{00a1}', '\u{1681}', '\u{180e}', '\u{200b}', '\u{200c}',
        '\u{2027}', '\u{202a}', '\u{202e}', '\u{2030}', '\u{205e}', '\u{2060}', '\u{3001}', '🦀',
    ] {
        cases.push(format!(" {ch} 42 {ch} "));
    }
    let mut source = String::from("use str; fn main() -> nil {");
    for text in cases {
        for (function, expected) in [
            ("trim", text.trim()),
            ("trim_start", text.trim_start()),
            ("trim_end", text.trim_end()),
        ] {
            source.push_str(&format!(
                "require(str::{function}({}) == {});\n",
                literal(&text),
                literal(expected)
            ));
        }
    }
    source.push('}');
    let artifact = compile(&source);
    let mut sink = vec![];
    let mut context = RuntimeContext::new(&mut sink, VmCapabilities::restricted());
    interpreter::run_with_context(&artifact, &mut context).unwrap();
    drop(context);
    assert!(sink.is_empty());
    assert!(
        artifact
            .imports
            .iter()
            .all(|import| !import.name().contains("trim") && !import.name().contains("replace"))
    );
}

#[test]
fn trim_imports_share_the_existing_native_string_namespace() {
    for imports in [
        "use str::*;",
        "use str::{trim, trim_start, trim_end, replace, to_int, builder, build};",
    ] {
        let artifact = compile(&format!(
            r#"{imports}
            use fmt;
            fn main() -> nil {{
                require(trim(" 42 ") == "42");
                require(trim_start(" 42 ") == "42 ");
                require(trim_end(" 42 ") == " 42");
                require(replace("path", "t", "T") == "paTh");
                require(to_int(trim(" 42 ")) == 42);
                var text = builder();
                fmt::write(text, "value={{}}", 42);
                require(build(text) == "value=42");
            }}"#
        ));
        interpreter::run_with(&artifact, &mut vec![]).unwrap();
    }
}

#[test]
fn replace_handles_non_overlapping_utf8_matches_and_empty_patterns() {
    let artifact = compile(
        r#"use str;
        fn main() -> nil {
            require(str::replace("one fish, two fish", "fish", "crab") == "one crab, two crab");
            require(str::replace("banana", "ana", "X") == "bXna");
            require(str::replace("α🦀β🦀", "🦀", "") == "αβ");
            require(str::replace("unchanged", "missing", "x") == "unchanged");
            require(str::replace("", "x", "replacement") == "");
            when str::replace("text", "", "prefix") is Error::InvalidArgument(operation) {
                require(operation == "str::replace");
            } else { crash("empty pattern was accepted"); }

            const failure = Error::MissingField("original");
            when str::replace(failure, "x", "y") is Error::MissingField(field) {
                require(field == "original");
            } else { crash("input Error was replaced"); }
        }"#,
    );
    interpreter::run_with(&artifact, &mut vec![]).unwrap();
    assert!(
        artifact
            .imports
            .iter()
            .all(|import| !import.name().contains("replace"))
    );
}

#[test]
fn replacement_output_respects_runtime_buffer_limits() {
    let artifact = compile(
        r#"use str;
        fn main() -> str { str::replace("abc", "a", "four") }"#,
    );
    let mut sink = vec![];
    let mut context = RuntimeContext::new(&mut sink, VmCapabilities::default());
    context.limits.max_buffer_bytes = Some(4);
    let error = interpreter::run_with_context(&artifact, &mut context).unwrap_err();
    assert_eq!(error.kind(), "ResourceLimit");
}

#[test]
fn trim_preserves_errors_and_surrounding_recovery() {
    let artifact = compile(
        r#"use str;
        fn fail() -> str { (1 / 0) as str }
        fn main() -> nil {
            const poisoned = fail();
            const result = str::trim(poisoned);
            when result is Error::DivisionByZero {} else { crash("lost original error"); }
            require((str::trim_start(fail()) ?! "fallback") == "fallback");
            try {
                str::trim_end(fail());
                crash("missed lexical Error");
            } catch(error) {
                when error is Error::DivisionByZero {} else { crash(error); }
            }
            require((str::trim(" 17 ") as int) == 17);
            const invalid = str::trim("prefix=17") as int;
            when invalid is Error::ParseFailure {} else { crash("trim removed non-whitespace"); }
        }"#,
    );
    interpreter::run_with(&artifact, &mut vec![]).unwrap();
}

#[test]
fn trim_buffer_failures_propagate_and_remain_catchable() {
    let artifact = compile(r#"use str; fn main() -> str { str::trim(" 7 ") }"#);
    let mut sink = vec![];
    let mut context = RuntimeContext::new(&mut sink, VmCapabilities::restricted());
    context.limits.max_buffer_bytes = Some(2);
    let error = interpreter::run_with_context(&artifact, &mut context).unwrap_err();
    assert_eq!(error.kind(), "ResourceLimit");

    let artifact = compile(
        r#"use str;
        fn main() -> nil {
            try { str::trim(" 7 "); crash("missed allocation failure"); }
            catch(error) {
                when error is Error::ResourceLimit {} else { crash(error); }
            }
            require(str::trim("7") == "7");
        }"#,
    );
    interpreter::run_with_context(&artifact, &mut context).unwrap();
}

#[test]
fn read_line_prompt_is_output_and_trim_only_removes_input_whitespace() {
    let artifact = compile(
        r#"use io; use str;
        fn main() -> nil {
            io::print("Enter number of rows: ");
            const input = io::read_line();
            const rows = str::trim(input) as int;
            io::println($"{rows}");
        }"#,
    );
    for (input, expected) in [
        ("12\n", "12"),
        ("12\r\n", "12"),
        (" \t-12 \n", "-12"),
        ("\u{00a0}12\u{3000}\n", "12"),
        ("12", "12"),
    ] {
        let mut sink = vec![];
        let mut context = RuntimeContext::with_input(
            &mut sink,
            Box::new(Cursor::new(input.as_bytes())),
            VmCapabilities::default(),
        );
        interpreter::run_with_context(&artifact, &mut context).unwrap();
        drop(context);
        assert_eq!(
            String::from_utf8(sink).unwrap(),
            format!("Enter number of rows: {expected}\n")
        );
    }
}

#[test]
fn trim_validates_argument_types_and_keeps_helpers_private() {
    let error = project::compile_source(
        "bad_trim.cog",
        "include std; use str; fn main() { str::trim(12); str::trim_start(nil); str::trim_end(false); str::replace(1, \"a\", \"b\"); }",
    )
    .unwrap_err()
    .to_string();
    assert_eq!(error.matches("error[E2001]").count(), 4, "{error}");

    let error = project::compile_source(
        "private_trim.cog",
        "include std; use str; fn main() { str::trim_impl(\" x \", true, true); }",
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("private to module"), "{error}");
}
