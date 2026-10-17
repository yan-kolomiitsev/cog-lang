use cog::{bytecode, interpreter, project};

fn run(source: &str) -> Result<String, String> {
    let source = format!("include std;\n{source}");
    let program = project::compile_source("generics.cog", &source)?;
    let program = bytecode::decode(&bytecode::encode(&program).map_err(|error| error.to_string())?)
        .map_err(|error| error.to_string())?;
    let mut output = Vec::new();
    interpreter::run_with(&program, &mut output).map_err(|error| error.to_string())?;
    String::from_utf8(output).map_err(|error| error.to_string())
}

#[test]
fn generic_functions_infer_and_specialize_nested_types() {
    assert_eq!(
        run(r#"
            use io;
            fn identity[T: type](value: T) -> T { value }
            fn first[T: type](values: T[][]) -> T[] { values[0] }
            fn tuple_first[T: type](value: (T, str)) -> T { value.0 }
            fn main() {
                io::println(identity(42))!!;
                io::println(identity("cog"))!!;
                io::println(first([[1, 2], [3]]))!!;
                io::println(tuple_first((7, "tuple")))!!;
            }
            "#)
        .unwrap(),
        "42\ncog\n[1, 2]\n7\n"
    );
}

#[test]
fn generic_structs_infer_types_and_specialize_field_access() {
    assert_eq!(
        run(r#"
            use io;
            struct Box[T: type] { value: T }
            fn main() {
                var second = Box(5);
                second.value += 1;
                const text = Box("hello");
                io::println($"{second.value}:{text.value}")!!;
            }
            "#)
        .unwrap(),
        "6:hello\n"
    );
}

#[test]
fn generic_extension_methods_infer_receivers_and_specialize_generic_methods() {
    assert_eq!(
        run(r#"
            use io;
            struct Box[T: type] { value: T }
            extend[T: type] Box[T] {
                fn get(self) -> T { self.value }
                fn replace[U: type](self, value: U) -> Box[U] { Box(value) }
            }
            fn make_box() -> Box[int] {
                io::print("receiver");
                Box(41)
            }
            fn extract[T: type](box: Box[T]) -> T { box.get() }
            fn main() {
                const number = Box(42);
                io::println(number.get())!!;
                io::println(extract(Box(7)))!!;
                const text = number.replace("cog");
                io::println(text.get())!!;
                io::println(number.replace("chain").get())!!;
                io::println(make_box().get())!!;
            }
            "#)
        .unwrap(),
        "42\n7\ncog\nchain\nreceiver41\n"
    );
}

#[test]
fn generic_extension_methods_check_concrete_types_after_inference() {
    let error = run(r#"
        struct Box[T: type] { value: T }
        extend[T: type] Box[T] {
            fn replace(self, value: T) -> Box[T] { Box(value) }
        }
        fn main() {
            const number = Box(42);
            number.replace("wrong");
        }
        "#)
    .unwrap_err();
    assert!(error.contains("expects int"), "{error}");
    assert!(error.contains("got str"), "{error}");
}

#[test]
fn exact_extensions_take_precedence_over_generic_extensions() {
    assert_eq!(
        run(r#"
            use io;
            struct Box[T: type] { value: T }
            extend[T: type] Box[T] {
                fn kind(self) -> str { "generic" }
            }
            extend Box[int] {
                fn kind(self) -> str { "concrete" }
            }
            fn main() {
                io::println(Box(1).kind())!!;
                io::println(Box("x").kind())!!;
            }
            "#)
        .unwrap(),
        "concrete\ngeneric\n"
    );
}

#[test]
fn overlapping_generic_extension_methods_are_ambiguous() {
    let error = run(r#"
        struct Pair[A: type, B: type] { first: A, second: B }
        extend[T: type] Pair[T, int] {
            fn value(self) -> int { self.second }
        }
        extend[T: type] Pair[int, T] {
            fn value(self) -> int { self.first }
        }
        fn main() { Pair(1, 2).value(); }
        "#)
    .unwrap_err();
    assert!(
        error.contains("multiple applicable generic extensions"),
        "{error}"
    );
}

#[test]
fn generic_structs_compose_with_type_annotations_and_generic_functions() {
    assert_eq!(
        run(r#"
            use io;
            struct Box[T: type] { value: T }
            fn unwrap[T: type](item: Box[T]) -> T { item.value }
            fn main() {
                const number: Box[int] = Box(41);
                const nested: Box[Box[int]] = Box(number);
                io::println(unwrap(nested.value) + 1)!!;
            }
            "#)
        .unwrap(),
        "42\n"
    );
}

#[test]
fn generic_struct_arrays_keep_their_full_type_arguments() {
    assert_eq!(
        run(r#"
            use io;
            struct Box[T: type] { value: T }
            fn main() {
                var numbers: Box[int][] = [Box(1)];
                numbers << Box(2);
                io::println(numbers[1].value)!!;
            }
            "#)
        .unwrap(),
        "2\n"
    );

    let error = run(r#"
        struct Box[T: type] { value: T }
        fn main() {
            var numbers: Box[int][] = [Box(1)];
            numbers << Box("wrong");
        }
        "#)
    .unwrap_err();
    assert!(error.contains("Box[int]"), "{error}");
    assert!(error.contains("Box[str]"), "{error}");
}

#[test]
fn generic_struct_field_types_are_checked_after_inference() {
    let error = run(r#"
        struct Box[T: type] { value: T }
        fn main() {
            var number = Box(5);
            number.value = "text";
        }
        "#)
    .unwrap_err();
    assert!(error.contains("declared as int"), "{error}");
}

#[test]
fn generic_struct_trait_bounds_are_checked_for_each_instantiation() {
    assert_eq!(
        run(r#"
            use io;
            trait Display { fn display(self) -> str; }
            extend int with Display {
                fn display(self) -> str { "number" }
            }
            struct Box[T: type + Display] { value: T }
            fn main() {
                const item = Box(7);
                io::println(item.value.display())!!;
            }
            "#)
        .unwrap(),
        "number\n"
    );

    let error = run(r#"
        trait Display { fn display(self) -> str; }
        struct Box[T: type + Display] { value: T }
        fn main() { Box("text"); }
        "#)
    .unwrap_err();
    assert!(
        error.contains("does not implement trait `Display`"),
        "{error}"
    );
}

#[test]
fn generic_structs_diagnose_missing_or_inconsistent_type_arguments() {
    let missing = run(r#"
        struct Marker[T: type] {}
        fn main() { Marker(); }
        "#)
    .unwrap_err();
    assert!(
        missing.contains("cannot infer generic type parameter `T`"),
        "{missing}"
    );

    let inconsistent = run(r#"
        struct Pair[T: type] { left: T, right: T }
        fn main() { Pair(1, "text"); }
        "#)
    .unwrap_err();
    assert!(
        inconsistent.contains("inferred as both int and str"),
        "{inconsistent}"
    );
}

#[test]
fn comptime_struct_type_parameters_work_in_the_program_shape_from_the_language_reference() {
    assert_eq!(
        run(r#"
            include std;
            use io;
            comptime fn factorial(n: int) {
                if n == 0 || n == 1 { return 1; }
                return n * factorial(n - 1);
            }
            comptime FACTORIAL = factorial(5);
            comptime N: type = int;
            struct Integer { value: N }
            struct Box[T: type] { value: T }
            fn main() {
                const first = Integer(42);
                const text = Box("hello");
                var second = Box(5);
                second.value += 1;
                io::println($"{first}{second}{text}{FACTORIAL}");
            }
            "#)
        .unwrap(),
        "{value: 42}{value: 6}{value: hello}120\n"
    );
}

#[test]
fn generic_array_helpers_preserve_mutability_effects() {
    assert_eq!(
        run(r#"
            use io;
            fn append[T: type](items: T[], value: T) { items << value; }
            fn main() {
                var values = [1];
                append(values, 2);
                io::println(values)!!;
            }
            "#)
        .unwrap(),
        "[1, 2]\n"
    );

    let error = run(r#"
        fn append[T: type](items: T[], value: T) { items << value; }
        fn main() {
            const values = [1];
            append(values, 2);
        }
        "#)
    .unwrap_err();
    assert!(error.contains("cannot mutate through const"), "{error}");

    let error = run(r#"
        use array;
        fn swap_first_two[T: type](items: T[]) { items.swap(0, 1)!!; }
        fn main() {
            const values = [1, 2];
            swap_first_two(values);
        }
        "#)
    .unwrap_err();
    assert!(error.contains("cannot mutate through const"), "{error}");

    let error = run(r#"
        use array;
        fn swap_first_two[T: type](items: T[]) { items.swap(0, 1)!!; }
        fn swap_via_helper[T: type](items: T[]) { swap_first_two(items); }
        fn main() {
            const values = [1, 2];
            swap_via_helper(values);
        }
        "#)
    .unwrap_err();
    assert!(error.contains("cannot mutate through const"), "{error}");

    let error = run(r#"
        use array;
        use ord::Ord;
        fn swap_first_two[T: type + Ord](items: T[]) { items.swap(0, 1)!!; }
        fn swap_when_nonempty[T: type + Ord](items: T[]) {
            if items.len() > 1 { swap_first_two(items)!!; }
        }
        fn main() {
            const values = [1, 2];
            swap_when_nonempty(values);
        }
        "#)
    .unwrap_err();
    assert!(error.contains("cannot mutate through const"), "{error}");

    let error = run(r#"
        use array;
        use ord::Ord;
        fn insertion_range[T: type + Ord](items: T[], low: int, high: int) {
            var index = low + 1;
            while index <= high {
                var position = index;
                while position > low {
                    if items[position - 1].compare(items[position]) > 0 {
                        items.swap(position - 1, position)!!;
                        position -= 1;
                    } else { break; }
                }
                index += 1;
            }
        }
        fn insertion[T: type + Ord](items: T[]) {
            if items.len() > 1 { insertion_range(items, 0, items.len() - 1)!!; }
        }
        fn main() {
            const values = [2, 1];
            insertion(values);
        }
        "#)
    .unwrap_err();
    assert!(error.contains("cannot mutate through const"), "{error}");
}

#[test]
fn generic_type_parameters_must_be_consistent_across_arguments() {
    let error = run(r#"
        fn same[T: type](left: T, right: T) -> T { left }
        fn main() { same(1, "text"); }
        "#)
    .unwrap_err();
    assert!(error.contains("inferred as both int and str"), "{error}");
}

#[test]
fn trait_bounds_check_implementations_and_dispatch_specialized_methods() {
    assert_eq!(
        run(r#"
            use io;
            trait Describe {
                fn describe(self) -> str;
            }
            extend int with Describe {
                fn describe(self) -> str { $"number {self}" }
            }
            fn describe[T: type + Describe](value: T) -> str {
                value.describe()
            }
            fn main() { io::println(describe(7))!!; }
            "#)
        .unwrap(),
        "number 7\n"
    );
}

#[test]
fn generic_trait_bounds_reject_types_without_an_implementation() {
    let error = run(r#"
        trait Describe { fn describe(self) -> str; }
        fn describe[T: type + Describe](value: T) -> str { "unused" }
        fn main() { describe(7); }
        "#)
    .unwrap_err();
    assert!(
        error.contains("does not implement trait `Describe`"),
        "{error}"
    );
}

#[test]
fn generic_functions_accept_multiple_trait_bounds() {
    assert_eq!(
        run(r#"
            use eq::Eq;
            use io;
            use ord::Ord;

            fn ordered_equal[T: type + Eq[T] + Ord](left: T, right: T) -> bool {
                left.equals(right) && left.compare(right) == 0
            }

            fn main() { io::println(ordered_equal(4, 4))!!; }
            "#)
        .unwrap(),
        "true\n"
    );
}

#[test]
fn parameterized_traits_substitute_method_and_bound_types() {
    assert_eq!(
        run(r#"
            use io;
            trait Convert[From: type, To: type] {
                fn convert(self, value: From) -> To;
            }
            struct Label { value: int }
            extend Label with Convert[int, str,] {
                fn convert(self, value: int) -> str {
                    $"label {self.value + value}"
                }
            }
            fn convert[T: type, U: type + Convert[int, T]](value: U, prefix: int, fallback: T) -> T {
                value.convert(prefix)
            }
            fn main() { io::println(convert(Label(7), 2, "unused"))!!; }
            "#)
        .unwrap(),
        "label 9\n"
    );
}

#[test]
fn parameterized_trait_implementations_check_arity_and_signatures() {
    let wrong_signature = run(r#"
        trait Convert[T: type] { fn convert(self) -> T; }
        struct Label { value: int }
        extend Label with Convert[str] {
            fn convert(self) -> int { self.value }
        }
        fn main() {}
        "#)
    .unwrap_err();
    assert!(
        wrong_signature.contains("does not match trait `Convert`"),
        "{wrong_signature}"
    );

    let wrong_arity = run(r#"
        trait Convert[T: type] { fn convert(self) -> T; }
        struct Label { value: int }
        extend Label with Convert {
            fn convert(self) -> int { self.value }
        }
        fn main() {}
        "#)
    .unwrap_err();
    assert!(
        wrong_arity.contains("expects 1 type argument(s), got 0"),
        "{wrong_arity}"
    );

    let wrong_bound = run(r#"
        trait Convert[T: type] { fn convert(self) -> T; }
        struct Label { value: int }
        extend Label with Convert[str] {
            fn convert(self) -> str { "label" }
        }
        fn as_int[T: type + Convert[int]](value: T) -> int { 0 }
        fn main() { as_int(Label(1)); }
        "#)
    .unwrap_err();
    assert!(
        wrong_bound.contains("does not implement trait `Convert[int]`"),
        "{wrong_bound}"
    );
}

#[test]
fn trait_implementations_use_self_in_signatures_and_generic_functions() {
    assert_eq!(
        run(r#"
            use io;
            trait Increment {
                fn increment(self, amount: int) -> Self;
            }
            struct Counter { value: int }
            extend Counter with Increment {
                fn increment(self, amount: int) -> Self {
                    Counter(self.value + amount)
                }
            }
            fn advance[T: type + Increment](value: T) -> T {
                value.increment(2)
            }
            fn main() { io::println(advance(Counter(5)))!!; }
            "#)
        .unwrap(),
        "{value: 7}\n"
    );
}

#[test]
fn trait_implementation_signatures_must_match_the_contract() {
    let error = run(r#"
        trait Describe { fn describe(self, radix: int) -> str; }
        extend int with Describe {
            fn describe(self, radix: str) -> str { "bad" }
        }
        fn main() {}
        "#)
    .unwrap_err();
    assert!(error.contains("does not match trait `Describe`"), "{error}");
}

#[test]
fn comptime_extend_can_implement_a_trait_for_a_resolved_type() {
    assert_eq!(
        run(r#"
            use io;

            trait Format[Output: type] {
                fn format(self) -> Output;
            }
            comptime Scalar: type = int;
            comptime Text: type = str;

            comptime extend Scalar with Format[Text] {
                fn format(self) -> str { $"value {self}" }
            }

            fn format[T: type + Format[Text]](value: T) -> str {
                value.format()
            }

            fn main() { io::println(format(12))!!; }
            "#)
        .unwrap(),
        "value 12\n"
    );
}

#[test]
fn obsolete_generic_declarations_have_a_migration_diagnostic() {
    for declaration in [
        "fn identity(comptime T: type, value: T) { value }",
        "struct Box(comptime T: type) { value: T }",
        "trait Measure(comptime T: type) { fn measure(self, value: T); }",
        "fn identity<T>(value: T) { value }",
    ] {
        let error = run(&format!("{declaration} fn main() {{}}")).unwrap_err();
        assert!(error.contains("[T: type]"), "{error}");
    }
}

#[test]
fn explicit_generic_arguments_specialize_functions_structs_and_nested_calls() {
    assert_eq!(
        run(r#"
        use io;
        struct Box[T: type] { value: T }
        struct Marker[T: type] {}
        fn identity[T: type](value: T) -> T { value }
        fn outer[T: type](value: T) -> T { identity[T](value) }
        fn main() {
            const number: Box[int] = Box[int](5);
            const nested: Box[Box[int][]] = Box[Box[int][]]([number]);
            const marker = Marker[str]();
            io::println(outer[int](nested.value[0].value));
            io::println(identity[str]("text"));
            io::println(marker);
        }
    "#)
        .unwrap(),
        "5\ntext\n{}\n"
    );
}

#[test]
fn explicit_generic_arguments_accept_array_types_and_comptime_type_aliases() {
    assert_eq!(
        run(r#"
        use io;
        comptime Number: type = int;
        fn identity[T: type](value: T) -> T { value }
        fn main() {
            const items = identity[int[]]([]);
            io::println(items);
            io::println(identity[Number](42));
        }
    "#)
        .unwrap(),
        "[]\n42\n"
    );
}

#[test]
fn bracket_resolution_respects_local_shadowing_and_chained_indices() {
    assert_eq!(
        run(r#"
        use io;
        struct Box[T: type] { value: T }
        fn main() {
            const grid = [[1, 2], [3, 4]];
            const Box = grid;
            io::println(Box[0][1]);
            io::println(grid[1][0]);
        }
    "#)
        .unwrap(),
        "2\n3\n"
    );
}

#[test]
fn explicit_generics_reject_wrong_arity_values_and_mismatched_types() {
    for call in [
        "identity[int, str](1)",
        "identity[1](1)",
        "identity[str](1)",
    ] {
        let error = run(&format!(
            "fn identity[T: type](value: T) -> T {{ value }} fn main() {{ {call}; }}"
        ))
        .unwrap_err();
        assert!(error.contains("error["), "{error}");
    }
    assert!(run("fn main() { const items = [1]; items[]; }").is_err());
}

#[test]
fn clone_supports_nested_arrays_tuples_and_generic_trait_calls() {
    assert_eq!(
        run(r#"
            use clone;
            use io;
            fn main() {
                const source: int[][] = [[1, 2], [3]];
                var copied = source.clone();
                copied[0] << 9;
                io::println(source)!!;
                io::println(copied)!!;

                const pair = (4, [5, 6]);
                io::println(clone::value(pair))!!;
                var pair_copy = pair.clone();
                pair_copy[1] << 8;
                io::println(pair)!!;
                io::println(pair_copy)!!;

                const row = [10];
                const graph = [row, row];
                var graph_copy = graph.clone();
                graph_copy[0] << 11;
                io::println(graph)!!;
                io::println(graph_copy)!!;
            }
            "#)
        .unwrap(),
        "[[1, 2], [3]]\n[[1, 2, 9], [3]]\n[4, [5, 6]]\n[4, [5, 6]]\n[4, [5, 6, 8]]\n[[10], [10]]\n[[10, 11], [10, 11]]\n"
    );
}

#[test]
fn generic_standard_sorting_accepts_user_defined_ordering_traits() {
    assert_eq!(
        run(r#"
            use io;
            use ord::Ord;
            use sort;

            struct Score { value: int }

            extend Score with Ord {
                fn compare(self, other: Score) -> int {
                    if self.value < other.value { return -1; }
                    if self.value > other.value { return 1; }
                    0
                }
            }

            fn main() {
                var scores = [Score(3), Score(1), Score(2)];
                sort::quick(scores);
                io::println(scores)!!;
            }
            "#)
        .unwrap(),
        "[{value: 1}, {value: 2}, {value: 3}]\n"
    );
}

#[test]
fn generic_clone_uses_custom_nominal_implementations() {
    assert_eq!(
        run(r#"
            use clone;
            use clone::Clone;
            use io;

            struct Point { x: int }

            extend Point with Clone {
                fn clone(self) -> Point { Point(self.x) }
            }

            fn main() { io::println(clone::value(Point(8)))!!; }
            "#)
        .unwrap(),
        "{x: 8}\n"
    );
}
