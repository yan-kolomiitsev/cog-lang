use cog::runtime::{RuntimeContext, VmCapabilities};
use cog::{bytecode, interpreter, project};

fn compile(source: &str) -> cog::backend::CompiledProgram {
    let source = format!(
        r#"include std; use array; use ascii; use base64; use bytes; use hex; use str; use err::Error;
        fn require(condition: bool) {{ if !condition {{ crash("assertion failed"); }} }}
        {source}"#
    );
    let program = project::compile_source("data-modules.cog", &source)
        .unwrap_or_else(|error| panic!("{error}"));
    bytecode::decode(&bytecode::encode(&program).unwrap()).unwrap()
}

fn run(source: &str) {
    let program = compile(source);
    let mut sink = Vec::new();
    interpreter::run_with_capabilities(&program, &mut sink, VmCapabilities::restricted()).unwrap();
    assert!(sink.is_empty());
    assert!(program.imports.iter().all(|import| {
        ![
            "array::",
            "ascii::",
            "base64::",
            "hex::",
            "str::split",
            "str::join",
        ]
        .iter()
        .any(|prefix| import.name().starts_with(prefix))
    }));
}

fn literal(text: &str) -> String {
    format!(
        "\"{}\"",
        text.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
            .replace('\t', "\\t")
            .replace('\0', "\\0")
    )
}

#[test]
fn array_queries_and_slices_cover_empty_values_and_checked_ranges() {
    run(r#"
        fn add_wrong(items) { items << "wrong" }
        fn type_error(value) {
            when value is Error::TypeMismatch {} else { crash("expected TypeMismatch"); }
        }
        fn main() {
            const items = [7, 1, 3, 7];
            const empty: int[] = [];
            const empty_words: str[] = [];
            require(array::is_empty(empty));
            require(!array::is_empty(items));
            require(array::first(items) == 7);
            require(array::last(items) == 7);
            require(array::first(empty) == nil);
            require(array::last(empty) == nil);
            require(array::index_of(items, 7) == 0);
            require(array::index_of(items, nil) == nil);
            require(array::index_of(items, "Cog") == nil);
            require(array::index_of(items, 9) == nil);
            require(!array::contains(items, nil));
            require(len(array::slice(items, 4, 0)) == 0);
            require(len(array::slice(empty, 0, 0)) == 0);
            require(len(array::reversed(empty)) == 0);
            require(len(array::concat(empty, empty)) == 0);
            type_error(add_wrong(items.empty_like()));
            type_error(add_wrong(array::slice(items, 4, 0)!!));
            type_error(add_wrong(array::reversed(empty)!!));
            type_error(add_wrong(array::concat(empty, empty)!!));
            type_error(array::concat(empty, empty_words));
            const selected = array::slice(items, offset: 1, count: 2)!!;
            require(selected[0] == 1 && selected[1] == 3);
            for index in 0..5 {
                const offsets = [-1, 5, 0, 3, 1];
            const counts = [1, 0, -1, 2, "9223372036854775807" as int];
                when array::slice(items, offsets[index] as int, counts[index] as int) is Error::InvalidArgument(op) {
                    require(op == "array::slice");
                } else { crash("invalid slice accepted"); }
            }
        }
    "#);
}

#[test]
fn array_results_have_fresh_outer_storage_and_preserve_nested_aliases() {
    run(r#"
        fn main() {
            const empty: int[] = [];
            const input = [1, 2, 3];
            var selected = array::slice(input, 1, 2)!!;
            selected[0] = 20;
            selected << 4;
            var backward = array::reversed(input)!!;
            backward[0] = 30;
            var combined = array::concat(input, [4])!!;
            combined << 5;
            require(input[1] == 2 && input[2] == 3 && len(input) == 3);
            require(selected[0] == 20 && selected[1] == 3 && selected[2] == 4);
            require(backward[0] == 30 && backward[2] == 1);
            require(combined[0] == 1 && combined[3] == 4 && combined[4] == 5);

            var nested = [[1], [2]];
            const selected_nested = array::slice(nested, 0, 1)!!;
            const reversed_nested = array::reversed(nested)!!;
            const empty_nested: int[][] = [];
            const combined_nested = array::concat(empty_nested, nested)!!;
            nested[0] << 9;
            require(selected_nested[0][1] == 9);
            require(reversed_nested[1][1] == 9);
            require(combined_nested[0][1] == 9);
        }
    "#);

    let error = project::compile_source(
        "readonly-elements.cog",
        r#"include std; use array; fn main() {
            const original = [[1]];
            var result = array::slice(original, 0, 1);
            result[0] << 2;
        }"#,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("read-only"), "{error}");
}

#[test]
fn array_utilities_validate_inputs_and_follow_language_equality() {
    run(r#"
        fn type_error(value) {
            when value is Error::TypeMismatch {} else { crash("expected TypeMismatch"); }
        }
        fn main() {
            const empty: int[] = [];
            type_error(array::is_empty(""));
            type_error(array::first(""));
            type_error(array::last(false));
            type_error(array::index_of(42, nil));
            type_error(array::slice("", 0, 0));
            type_error(array::reversed("abc"));
            type_error(array::concat(empty, ""));
            require(array::contains([1], 1.0));
            require(!array::contains([1], "1"));
            type_error(array::contains([[1]], [1]));
        }
    "#);
}

#[test]
fn array_utilities_propagate_errors_as_they_consume_elements() {
    run(r#"
        fn preserved(value) {
            when value is Error::MissingField(field) { require(field == "marker"); }
            else { crash("original error was lost"); }
        }
        fn main() {
            const empty: int[] = [];
            const failure = Error::MissingField("marker");
            preserved(array::first([failure]));
            preserved(array::last([failure]));
            preserved(array::slice([failure], 0, 1));
            preserved(array::reversed([failure]));
            preserved(array::concat(empty, [failure]));
            preserved(array::index_of([failure], 7));
            preserved(array::contains(empty, failure));
            preserved(array::slice(failure, 0, 0));
            require(len(array::slice([failure], 0, 0)) == 0);
            require(array::index_of([7, failure], 7) == 0);
            require(!array::is_empty([failure]));
        }
    "#);
}

#[test]
fn ascii_classification_and_case_conversion_cover_every_byte() {
    let mut source = String::from("fn main() {\n");
    for value in 0..=255u8 {
        for (function, expected) in [
            ("is_ascii", value.is_ascii()),
            ("is_alpha", value.is_ascii_alphabetic()),
            ("is_digit", value.is_ascii_digit()),
            ("is_alphanumeric", value.is_ascii_alphanumeric()),
            ("is_lowercase", value.is_ascii_lowercase()),
            ("is_uppercase", value.is_ascii_uppercase()),
            ("is_whitespace", matches!(value, 9..=13 | 32)),
        ] {
            source.push_str(&format!(
                "require(ascii::{function}({value}) == {expected});\n"
            ));
        }
        for (function, expected) in [
            ("to_lowercase", value.to_ascii_lowercase()),
            ("to_uppercase", value.to_ascii_uppercase()),
        ] {
            source.push_str(&format!(
                "require(ascii::{function}({value}) == {expected});\n"
            ));
        }
    }
    source.push('}');
    run(&source);
}

#[test]
fn ascii_rejects_out_of_range_bytes_and_preserves_errors() {
    run(r#"
        fn main() {
            when ascii::is_digit(-1) is Error::InvalidArgument {} else { crash("negative byte"); }
            when ascii::to_uppercase(256) is Error::InvalidArgument {} else { crash("large byte"); }
            when ascii::is_alpha(1 / 0) is Error::DivisionByZero {} else { crash("lost Error"); }
            try { ascii::to_lowercase(-1); crash("missed Error"); }
            catch(error) { when error is Error::InvalidArgument {} else { crash(error); } }
        }
    "#);
}

#[test]
fn hex_encoding_matches_known_text_and_round_trips_every_byte() {
    let lower = (0..=255u8)
        .map(|value| format!("{value:02x}"))
        .collect::<String>();
    run(&format!(
        r#"
        fn main() {{
            var input = bytes::new(256)!!;
            for index in 0..256 {{ bytes::set(input, index, index)!!; }}
            require(hex::encode(input) == "{lower}");
            require(hex::encode(input, uppercase: true) == "{}");
            const decoded = hex::decode("{lower}")!!;
            for index in 0..256 {{ require(bytes::get(decoded, index) == index); }}
            require(hex::encode(bytes::from_str("Cog")) == "436f67");
            require(hex::encode(hex::decode("aAbBcCdDeEfF")) == "aabbccddeeff");
            require(hex::encode(bytes::new(0)) == "");
            require(bytes::len(hex::decode("")) == 0);
            var writable = hex::decode("00ff")!!;
            bytes::set(writable, 0, 42)!!;
            require(bytes::get(writable, 0) == 42);
        }}
    "#,
        lower.to_ascii_uppercase()
    ));
}

#[test]
fn hex_rejects_malformed_text_and_keeps_decode_separate_from_utf8() {
    let mut source = String::from("fn main() {\n");
    for text in ["0", "abc", "0x00", " 00", "00\n", "zz", "é", "000g"] {
        source.push_str(&format!(
            "when hex::decode({}) is Error::ParseFailure(from, to) {{\n\
             require(from == \"hex\" && to == \"bytes\");\n\
             }} else {{ crash(\"invalid hexadecimal text accepted\"); }}\n",
            literal(text)
        ));
    }
    source.push_str(
        r#"
            const binary = hex::decode("ff")!!;
            when bytes::to_str(binary, 0, 1) is Error::ParseFailure {} else { crash("invalid UTF-8"); }
            when hex::encode(1 / 0) is Error::DivisionByZero {} else { crash("lost Error"); }
            when hex::decode((1 / 0) as str) is Error::DivisionByZero {} else { crash("lost Error"); }
        }"#,
    );
    run(&source);
}

#[test]
fn string_search_split_and_join_match_utf8_text_without_normalization() {
    let cases = [
        ("", ","),
        (",", ","),
        (",a,,b,", ","),
        ("aaaaa", "aa"),
        ("é🦀α🦀", "🦀"),
        ("é é", "é"),
        ("a\0b\0", "\0"),
        ("first\r\nsecond\r\n", "\r\n"),
        ("a{}b", "{}"),
        ("short", "longer separator"),
    ];
    let mut source = String::from("fn main() {\n");
    for (index, (text, separator)) in cases.into_iter().enumerate() {
        for (function, expected) in [
            ("starts_with", text.starts_with(separator)),
            ("ends_with", text.ends_with(separator)),
            ("contains", text.contains(separator)),
        ] {
            source.push_str(&format!(
                "require(str::{function}({}, {}) == {expected});\n",
                literal(text),
                literal(separator)
            ));
        }
        let parts = text.split(separator).collect::<Vec<_>>();
        source.push_str(&format!(
            "const parts{index} = str::split({}, {})!!;\nrequire(len(parts{index}) == {});\n",
            literal(text),
            literal(separator),
            parts.len()
        ));
        for (part_index, part) in parts.iter().enumerate() {
            source.push_str(&format!(
                "require(parts{index}[{part_index}] == {});\n",
                literal(part)
            ));
        }
        source.push_str(&format!(
            "require(str::join(parts{index}, {}) == {});\n",
            literal(separator),
            literal(text)
        ));
    }
    source.push_str(
        r#"
            require(str::starts_with("", ""));
            require(str::ends_with("Cog", ""));
            require(str::contains("", ""));
            const empty: str[] = [];
            require(str::join(empty) == "");
            require(str::join(["a", "b"]) == "ab");
            require(str::join(["a"], "---") == "a");
            var fields = str::split("a,b", ",")!!;
            fields[0] = "x";
            fields << "c";
            require(str::join(fields, "/") == "x/b/c");
            when str::split("Cog", "") is Error::InvalidArgument(op) {
                require(op == "str::split");
            } else { crash("empty separator accepted"); }
            when str::join([42]) is Error::TypeMismatch {} else { crash("non-string part"); }
            when str::join("") is Error::TypeMismatch {} else { crash("non-array input"); }
            when str::join([1 / 0]) is Error::DivisionByZero {} else { crash("lost part Error"); }
            when str::contains("Cog", (1 / 0) as str) is Error::DivisionByZero {} else { crash("lost Error"); }
            when str::split((1 / 0) as str, ",") is Error::DivisionByZero {} else { crash("lost Error"); }
        }"#,
    );
    run(&source);
}

#[test]
fn text_and_hex_helpers_obey_existing_buffer_limits_and_recover() {
    let program = compile(
        r#"
        fn limited(value) {
            when value is Error::ResourceLimit {} else { crash("buffer limit bypassed"); }
        }
        fn main() {
            limited(hex::encode(bytes::from_str("ab")));
            limited(hex::decode("0000"));
            limited(str::contains("four", "f"));
            limited(str::split("four", ","));
            limited(str::join(["ab", "cd"]));
            require(hex::encode(bytes::from_str("a")) == "61");
            require(str::join(["a", "b"], "-") == "a-b");
        }"#,
    );
    let mut sink = Vec::new();
    let mut context = RuntimeContext::new(&mut sink, VmCapabilities::restricted());
    context.limits.max_buffer_bytes = Some(3);
    interpreter::run_with_context(&program, &mut context).unwrap();
}

#[test]
fn new_modules_support_selective_and_wildcard_imports_with_defaults() {
    for imports in [
        "use hex::*; use array::{slice, first}; use ascii::to_lowercase as lowercase;",
        "use hex::{encode, decode}; use array::*; use ascii::*; use ascii::to_lowercase as lowercase;",
    ] {
        run(&format!(
            r#"{imports}
            fn main() {{
                require(encode(decode("Ab"), uppercase: true) == "AB");
                require(first(slice([4, 5], count: 1, offset: 1)) == 5);
                require(lowercase(65) == 97);
            }}"#
        ));
    }
}

#[test]
fn base64_encodes_and_decodes_known_vectors_with_all_padding_lengths() {
    let mut source = String::from("fn main() {\n");
    for (plain, encoded) in [
        ("", ""),
        ("f", "Zg=="),
        ("fo", "Zm8="),
        ("foo", "Zm9v"),
        ("foob", "Zm9vYg=="),
        ("fooba", "Zm9vYmE="),
        ("foobar", "Zm9vYmFy"),
        ("Cog", "Q29n"),
        ("café 🦀\0", "Y2Fmw6kg8J+mgAA="),
    ] {
        source.push_str(&format!(
            "require(base64::encode(bytes::from_str({})) == {});\n\
             require(base64::encode(base64::decode({})) == {});\n\
             require(bytes::to_str(base64::decode({}), 0, bytes::len(bytes::from_str({}))) == {});\n",
            literal(plain),
            literal(encoded),
            literal(encoded),
            literal(encoded),
            literal(encoded),
            literal(plain),
            literal(plain)
        ));
    }
    source.push('}');
    run(&source);
}

#[test]
fn base64_handles_binary_data_and_returns_writable_buffers() {
    // Known encoding of bytes 0..255, independent of the Cog implementation.
    const ENCODED: &str = concat!(
        "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8gISIjJCUmJygpKissLS4v",
        "MDEyMzQ1Njc4OTo7PD0+P0BBQkNERUZHSElKS0xNTk9QUVJTVFVWV1hZWltcXV5f",
        "YGFiY2RlZmdoaWprbG1ub3BxcnN0dXZ3eHl6e3x9fn+AgYKDhIWGh4iJiouMjY6P",
        "kJGSk5SVlpeYmZqbnJ2en6ChoqOkpaanqKmqq6ytrq+wsbKztLW2t7i5uru8vb6/",
        "wMHCw8TFxsfIycrLzM3Oz9DR0tPU1dbX2Nna29zd3t/g4eLj5OXm5+jp6uvs7e7v",
        "8PHy8/T19vf4+fr7/P3+/w=="
    );
    run(&format!(
        r#"
        fn main() {{
            var original = bytes::new(256)!!;
            for index in 0..256 {{ bytes::set(original, index, index)!!; }}
            const view = original;
            require(base64::encode(view) == "{ENCODED}");
            var decoded = base64::decode("{ENCODED}")!!;
            require(bytes::len(decoded) == 256);
            for index in 0..256 {{ require(bytes::get(decoded, index) == index); }}
            bytes::set(decoded, 0, 42)!!;
            require(bytes::get(original, 0) == 0);
            require(bytes::get(decoded, 0) == 42);
            require(base64::encode(base64::decode("+/8=")) == "+/8=");
            when bytes::to_str(base64::decode("/w=="), 0, 1) is Error::ParseFailure {{}}
            else {{ crash("binary decode implied valid UTF-8"); }}
            for length in 0..260 {{
                var input = bytes::new(length)!!;
                for index in 0..length {{ bytes::set(input, index, index % 256)!!; }}
                const output = base64::decode(base64::encode(input)!!)!!;
                require(bytes::len(output) == length);
                for index in 0..length {{ require(bytes::get(output, index) == index % 256); }}
            }}
        }}
    "#
    ));
}

#[test]
fn base64_rejects_invalid_lengths_digits_and_padding_with_parse_failure() {
    let mut source = String::from("fn main() {\n");
    for text in [
        "A", "AA", "AAA", "AAAAA", "Zg", "Zg=", "Zg===", "Zm8", "Zg==\n", " Zg==   ", "AA A",
        "AA\tA", "AA\rA", "AA\nA", "AA\0A", "éAA", "____", "----", "!!!!", "====", "=AAA", "A=AA",
        "AA=A", "A===", "AA==AAAA", "AAA=AAAA", "AAAA=AAA", "AAAAAA=A",
    ] {
        source.push_str(&format!(
            "when base64::decode({}) is Error::ParseFailure(from, to) {{\n\
             require(from == \"base64\" && to == \"bytes\");\n\
             }} else {{ crash(\"invalid Base64 accepted\"); }}\n",
            literal(text)
        ));
    }
    source.push_str(
        r#"
            try { base64::decode("AA=A"); crash("missed parse failure"); }
            catch(error) { when error is Error::ParseFailure {} else { crash(error); } }
        }"#,
    );
    run(&source);
}

#[test]
fn base64_requires_zero_unused_bits_for_both_padded_forms() {
    // Valid alphabets do not make the low bits of a padded final sextet valid.
    // Exercise every possible final sextet, rather than a few selected typos.
    const ALPHABET: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut source = String::from("fn main() {\n");
    for (value, digit) in ALPHABET.chars().enumerate() {
        for (encoded, valid) in [
            (format!("A{digit}=="), value % 16 == 0),
            (format!("AA{digit}="), value % 4 == 0),
        ] {
            if valid {
                source.push_str(&format!(
                    "require(base64::encode(base64::decode(\"{encoded}\")) == \"{encoded}\");\n"
                ));
            } else {
                source.push_str(&format!(
                    "when base64::decode(\"{encoded}\") is Error::ParseFailure {{}}\n\
                     else {{ crash(\"noncanonical padding bits accepted\"); }}\n"
                ));
            }
        }
    }
    source.push('}');
    run(&source);
}

#[test]
fn base64_preserves_input_errors_and_checks_buffer_limits() {
    run(r#"
        fn preserved(value) {
            when value is Error::MissingField(field) { require(field == "marker"); }
            else { crash("original error was lost"); }
        }
        fn main() {
            const empty: int[] = [];
            const failure = Error::MissingField("marker");
            preserved(base64::encode(failure));
            preserved(base64::decode(failure as str));
            when base64::encode(empty) is Error::TypeMismatch {} else { crash("non-byte input"); }
        }
    "#);

    let program = compile(
        r#"
        fn main() {
            when base64::encode(bytes::from_str("a")) is Error::ResourceLimit {}
            else { crash("encoded size bypassed buffer limit"); }
            when base64::decode("YQ==") is Error::ResourceLimit {}
            else { crash("input copy bypassed buffer limit"); }
            require(base64::encode(bytes::new(0)) == "");
            require(bytes::len(base64::decode("")) == 0);
        }"#,
    );
    let mut sink = Vec::new();
    let mut context = RuntimeContext::new(&mut sink, VmCapabilities::restricted());
    context.limits.max_buffer_bytes = Some(3);
    interpreter::run_with_context(&program, &mut context).unwrap();

    let program = compile(
        r#"
        fn main() {
            require(base64::encode(bytes::from_str("f")) == "Zg==");
            require(base64::encode(bytes::from_str("fo")) == "Zm8=");
            require(base64::encode(bytes::from_str("foo")) == "Zm9v");
            require(bytes::len(base64::decode("Zg==")) == 1);
            require(bytes::len(base64::decode("Zm8=")) == 2);
            require(bytes::len(base64::decode("Zm9v")) == 3);
            when base64::encode(bytes::from_str("foob")) is Error::ResourceLimit {}
            else { crash("expanded output bypassed limit"); }
        }"#,
    );
    context.limits.max_buffer_bytes = Some(4);
    interpreter::run_with_context(&program, &mut context).unwrap();
}

#[test]
fn base64_selective_alias_and_wildcard_imports_keep_private_helpers() {
    for (imports, encode, decode) in [
        ("use base64::*;", "encode", "decode"),
        ("use base64::{encode, decode};", "encode", "decode"),
        ("use base64 as codec;", "codec::encode", "codec::decode"),
    ] {
        run(&format!(
            r#"{imports} fn main() {{
                require({encode}(bytes::from_str("Cog")) == "Q29n");
                require(bytes::to_str({decode}("Q29n"), 0, 3) == "Cog");
            }}"#
        ));
    }
}
