use cog::error::RuntimeError;
use cog::runtime::VmCapabilities;
use cog::{bytecode, interpreter, project};

fn execute(source: &str, caps: VmCapabilities) -> (String, Result<(), RuntimeError>) {
    let source =
        format!("include std; use io; use math; use random; use sort; use err::Error;\n{source}");
    let program = project::compile_source("standard-library.cog", &source)
        .unwrap_or_else(|error| panic!("{error}"));
    let program = bytecode::decode(&bytecode::encode(&program).unwrap()).unwrap();
    let mut output = Vec::new();
    let result = interpreter::run_with_capabilities(&program, &mut output, caps);
    (String::from_utf8(output).unwrap(), result)
}

fn run(source: &str) -> String {
    let (output, result) = execute(source, VmCapabilities::default());
    result.unwrap();
    output
}

#[test]
fn math_domain_errors_are_recoverable_and_preserve_error_payloads() {
    assert_eq!(
        run(r#"
        fn invalid(value) {
            when value is Error::InvalidArgument(op) { io::println(op)!!; }
            else { crash("expected InvalidArgument"); }
        }
        fn main() {
            invalid(math::pow(2, -1));
            invalid(math::sqrt(-1));
            invalid(math::ln(0));
            invalid(math::ln(-1));
            try { math::sqrt(-1); crash("domain error was not caught"); }
            catch(error) { io::println(error.error_kind())!!; }
            when math::sqrt(Error::MissingField("marker")) is Error::MissingField(field) {
                io::println(field)!!;
            } else { crash("original Error was replaced"); }
            when math::pow(2, 1.5) is Error::TypeMismatch(op) { io::println(op)!!; }
            else { crash("fractional exponent accepted"); }
            when math::sqrt("4") is Error::TypeMismatch(op) { io::println(op)!!; }
            else { crash("nonnumeric argument accepted"); }
        }
    "#),
        "math::pow\nmath::sqrt\nmath::ln\nmath::ln\nInvalidArgument\nmarker\nmath::pow\nmath::sqrt\n"
    );
}

#[test]
fn random_validates_range_bounds_before_requesting_entropy() {
    let caps = VmCapabilities {
        allow_print: true,
        ..VmCapabilities::restricted()
    };
    let (output, result) = execute(
        r#"
        fn invalid(value) {
            when value is Error::InvalidArgument(op) { io::println(op)!!; }
            else { crash("invalid range reached entropy or arithmetic"); }
        }
        fn main() {
            invalid(random::range(3, 3));
            invalid(random::range(4, 3));
            invalid(random::range(0, 2147483649));
            when random::range(1.5, 3) is Error::TypeMismatch(op) { io::println(op)!!; }
            else { crash("noninteger bound accepted"); }
            when random::range(1, 3) is Error::PermissionDenied(op) { io::println(op)!!; }
            else { crash("valid range did not request entropy"); }
            const low = "-9223372036854775808" as int;
            const high = "9223372036854775807" as int;
            when random::range(low, high) is Error::Overflow { io::println("overflow")!!; }
            else { crash("range width did not preserve Overflow"); }
        }
    "#,
        caps,
    );
    result.unwrap();
    assert_eq!(
        output,
        "random::range\nrandom::range\nrandom::range\nrandom::range\nentropy::bytes\noverflow\n"
    );
}

#[test]
fn math_preserves_integer_boundaries_rounding_and_checked_powers() {
    assert_eq!(
        run(r#"
        fn main() {
            const largest = "9223372036854775807" as int;
            const smallest = "-9223372036854775808" as int;
            io::println(math::floor(-3.5))!!;
            io::println(math::ceil(-3.5))!!;
            io::println(math::round(-1.5))!!;
            io::println(math::round(-1.6))!!;
            io::println(math::round(2.6))!!;
            io::println(math::round(largest))!!;
            io::println(math::round(smallest))!!;
            io::println(math::pow(largest, 1))!!;
            io::println(math::pow(2, 62))!!;
            io::println(math::pow(-2, 3))!!;
            io::println(math::pow(-2, 4))!!;
            io::println(math::pow(0, 0))!!;
            io::println(when math::pow(2.0, 0) is float { true } else { false })!!;
            io::println(when math::pow(2, 63) is Error::Overflow { true } else { false })!!;
            io::println(when math::abs(smallest) is Error::Overflow { true } else { false })!!;
            io::println(when math::floor("inf" as float) is Error::Overflow { true } else { false })!!;
        }
    "#),
        "-4\n-3\n-1\n-2\n3\n9223372036854775807\n-9223372036854775808\n9223372036854775807\n4611686018427387904\n-8\n16\n1\ntrue\ntrue\ntrue\ntrue\n"
    );
}

#[test]
fn square_roots_and_logs_cover_small_and_large_finite_values() {
    let inputs = [
        f64::from_bits(1),
        f64::MIN_POSITIVE,
        1e-300,
        0.125,
        1.0,
        1.0 + f64::EPSILON,
        2.0,
        4.0,
        1e300,
        f64::MAX,
    ];
    let mut source = String::from("fn main() {\n");
    for input in inputs {
        source.push_str(&format!(
            "io::println(math::sqrt(\"{input:e}\" as float)!!)!!;\n\
             io::println(math::ln(\"{input:e}\" as float)!!)!!;\n"
        ));
    }
    source.push('}');
    let output = run(&source);
    let mut results = output.lines().map(|line| line.parse::<f64>().unwrap());
    for input in inputs {
        let sqrt = results.next().unwrap();
        let expected = input.sqrt();
        assert!(
            ((sqrt - expected) / expected).abs() <= 4.0 * f64::EPSILON,
            "sqrt({input:e}): {sqrt:e}, expected {expected:e}"
        );
        let ln = results.next().unwrap();
        let expected = input.ln();
        assert!(
            (ln - expected).abs() <= 1e-12,
            "ln({input:e}): {ln:e}, expected {expected:e}"
        );
    }
    assert!(results.next().is_none());
}

#[test]
fn nonfinite_math_inputs_terminate_and_preserve_numeric_results() {
    assert_eq!(
        run(r#"
        fn main() {
            const infinite = "inf" as float;
            const nan = "NaN" as float;
            io::println(math::sqrt(infinite))!!;
            io::println(math::ln(infinite))!!;
            io::println(math::log2(infinite))!!;
            io::println(math::log10(infinite))!!;
            io::println(math::sqrt(nan))!!;
            io::println(math::ln(nan))!!;
            io::println(when math::sqrt(-infinite) is Error::InvalidArgument { true } else { false })!!;
            io::println(when math::ln(-infinite) is Error::InvalidArgument { true } else { false })!!;
        }
    "#),
        "inf\ninf\ninf\ninf\nNaN\nNaN\ntrue\ntrue\n"
    );
}

#[test]
fn random_values_remain_within_the_requested_half_open_interval() {
    assert_eq!(
        run(r#"
        fn main() {
            for i in 0..256 {
                const value = random::range(-7, 11)!!;
                if value < -7 || value >= 11 { crash("random value outside bounds"); }
                when random::boolean()!! is bool {} else { crash("expected bool"); }
                if random::range(4, 5)!! != 4 { crash("single-value range failed"); }
                const full = random::range(0, 2147483648)!!;
                if full < 0 || full >= 2147483648 { crash("full-width range failed"); }
            }
            io::println("bounded")!!;
        }
    "#),
        "bounded\n"
    );
}

#[test]
fn in_place_sorts_return_nil_and_sorted_returns_an_independent_copy() {
    for algorithm in ["insertion", "stable", "selection", "bubble", "quick"] {
        assert_eq!(
            run(&format!(
                r#"
                fn main() {{
                    var values = [3, -1, 2, 3, 0];
                    const view = values;
                    const result: nil = sort::{algorithm}(values);
                    if result != nil || !sort::is_sorted(values) {{ crash("sort result or order incorrect"); }}
                    values << 9;
                    io::println(view)!!;

                    const copy = sort::sorted(values);
                    if !sort::is_sorted(copy) {{ crash("sorted copy is not ordered"); }}
                    values << 10;
                    io::println(values)!!;
                    io::println(copy)!!;

                    var empty: int[] = [];
                    const empty_result: nil = sort::{algorithm}(empty);
                    if empty_result != nil || !sort::is_sorted(empty) {{ crash("empty sort failed"); }}
                    io::println(empty.len())!!;

                    const singleton = sort::sorted([7]);
                    io::println(singleton)!!;
                }}
            "#
            )),
            "[-1, 0, 2, 3, 3, 9]\n[-1, 0, 2, 3, 3, 9, 10]\n[-1, 0, 2, 3, 3, 9]\n0\n[7]\n",
            "{algorithm}"
        );
    }
}

#[test]
fn in_place_sorts_require_mutable_bindings() {
    for algorithm in ["insertion", "stable", "selection", "bubble", "quick"] {
        let source = format!(
            "include std; use sort; fn main() {{ const values = [2, 1]; sort::{algorithm}(values); }}"
        );
        let diagnostic = project::compile_source("readonly-sort.cog", &source)
            .unwrap_err()
            .to_string();
        assert!(
            diagnostic.contains("cannot mutate through const/read-only access"),
            "{algorithm}: {diagnostic}"
        );
    }
}

#[test]
fn quick_sort_handles_ordered_reverse_duplicate_and_large_inputs() {
    assert_eq!(
        run(r#"
            use array;
            fn main() {
                var ascending: int[] = [];
                var descending: int[] = [];
                var equal: int[] = [];
                for index in 0..4096 {
                    ascending << index;
                    descending << (4095 - index);
                    equal << 8;
                }
                sort::quick(ascending);
                sort::quick(descending);
                sort::quick(equal);
                if !sort::is_sorted(ascending) || !sort::is_sorted(descending) || !sort::is_sorted(equal) {
                    crash("large quick sort failed");
                }
                io::println(ascending[0])!!;
                io::println(ascending[4095])!!;
                io::println(descending[0])!!;
                io::println(descending[4095])!!;
                io::println(equal.len())!!;
            }
        "#),
        "0\n4095\n0\n4095\n4096\n"
    );
}

#[test]
fn comparison_errors_propagate_and_leave_sort_inputs_as_permutations() {
    for algorithm in ["insertion", "stable", "selection", "bubble", "quick"] {
        assert_eq!(
            run(&format!(
                r#"
                use ord::Ord;
                struct Item {{ value: int }}
                var fail_value = -1;
                extend Item with Ord {{
                    fn compare(self, other: Self) -> int {{
                        if self.value == fail_value || other.value == fail_value {{
                            return Error::MissingField("compare marker");
                        }}
                        if self.value < other.value {{ return -1; }}
                        if self.value > other.value {{ return 1; }}
                        0
                    }}
                }}
                fn require(condition: bool) {{ if !condition {{ crash("sort check failed"); }} }}
                fn check_permutation(items: Item[]) {{
                    require(items.len() == 20);
                    var counts = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
                    for item in items {{ counts[item.value] += 1; }}
                    for value in 1..21 {{ require(counts[value] == 1); }}
                }}
                fn main() {{
                    for failing in 1..21 {{
                        fail_value = failing;
                        var items: Item[] = [];
                        for index in 0..20 {{ items << Item(value: 20 - index); }}
                        const caught = try {{ sort::{algorithm}(items); false }} catch(error) {{
                            when error is Error::MissingField(field) {{ field == "compare marker" }}
                            else {{ false }}
                        }};
                        require(caught);
                        check_permutation(items);
                    }}
                }}
            "#
            )),
            "",
            "{algorithm}"
        );
    }
}

#[test]
fn quick_sort_propagates_errors_from_its_recursive_partition_ranges() {
    assert_eq!(
        run(r#"
            use ord::Ord;
            struct Item { value: int }
            var compare_count = 0;
            extend Item with Ord {
                fn compare(self, other: Self) -> int {
                    compare_count += 1;
                    if compare_count == 24 { return Error::MissingField("recursive marker"); }
                    if self.value < other.value { return -1; }
                    if self.value > other.value { return 1; }
                    0
                }
            }
            fn require(condition: bool) { if !condition { crash("quick recursion check failed"); } }
            fn main() {
                var items: Item[] = [];
                for index in 0..20 { items << Item(value: 20 - index); }
                try { sort::quick(items); crash("recursive compare error was lost"); }
                catch(error) {
                    when error is Error::MissingField(field) {
                        require(field == "recursive marker");
                    } else { crash("quick replaced recursive compare error"); }
                }
                require(compare_count == 24);
                require(items.len() == 20);
                var counts = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
                for item in items { counts[item.value] += 1; }
                for value in 1..21 { require(counts[value] == 1); }
            }
        "#),
        ""
    );
}

#[test]
fn quick_sort_custom_ordering_handles_large_and_all_equal_arrays() {
    assert_eq!(
        run(r#"
            use ord::Ord;
            struct Number { value: int }
            extend Number with Ord {
                fn compare(self, other: Self) -> int {
                    if self.value < other.value { return -1; }
                    if self.value > other.value { return 1; }
                    0
                }
            }
            fn main() {
                var descending: Number[] = [];
                var equal: Number[] = [];
                for index in 0..1024 {
                    descending << Number(value: 1023 - index);
                    equal << Number(value: 5);
                }
                sort::quick(descending);
                sort::quick(equal);
                if !sort::is_sorted(descending) || !sort::is_sorted(equal) {
                    crash("generic quick sort failed");
                }
                io::println(descending[0]!!.value)!!;
                io::println(descending[1023]!!.value)!!;
                io::println(equal.len())!!;
            }
        "#),
        "0\n1023\n1024\n"
    );
}

#[test]
fn float_sort_reports_nan_as_unordered_and_array_swap_is_checked() {
    assert_eq!(
        run(r#"
            use array;
            fn main() {
                var values = [3.0, 1.0, 2.0];
                const nan: float = "NaN" as float;
                values[1] = nan;
                try { sort::quick(values); crash("NaN sort succeeded"); }
                catch(error) {
                    when error is Error::Unordered { io::println("unordered")!!; }
                    else { error!!; }
                }
                io::println(values.len())!!;
                var pair = [1, 2];
                try { pair.swap(-1, 0); crash("invalid swap succeeded"); }
                catch(error) { io::println(error.error_kind())!!; }
                io::println(pair)!!;
            }
        "#),
        "unordered\n3\nIndexOutOfBounds\n[1, 2]\n"
    );
}

#[test]
fn sorted_clones_user_defined_ordered_elements_and_the_outer_array() {
    assert_eq!(
        run(r#"
            use clone::Clone;
            use ord::Ord;
            struct Score { value: int }
            extend Score with Ord {
                fn compare(self, other: Self) -> int {
                    if self.value < other.value { return -1; }
                    if self.value > other.value { return 1; }
                    0
                }
            }
            extend Score with Clone {
                fn clone(self) -> Self { Score(value: self.value) }
            }
            fn main() {
                const scores = [Score(value: 3), Score(value: 1), Score(value: 2)];
                var result = sort::sorted(scores);
                io::println(result[0]!!.value)!!;
                io::println(scores[0]!!.value)!!;
                result << Score(value: 0);
                io::println(result.len())!!;
                io::println(scores.len())!!;
            }
        "#),
        "1\n3\n4\n3\n"
    );
}

#[test]
fn native_sort_returns_an_error_element_without_mutating_its_array() {
    for algorithm in ["quick", "stable"] {
        let source = r#"
            fn main() {
                const poison = 1 / 0;
                var values = [3, poison, 1];
                try { sort::SORT(values); crash("poisoned array sorted"); }
                catch(error) { io::println(error.error_kind())!!; }
                io::println(values[0])!!;
                when values[1] is Error::DivisionByZero { io::println("same error")!!; }
                else { crash("array error was changed"); }
                io::println(values[2])!!;
            }
        "#
        .replace("sort::SORT", &format!("sort::{algorithm}"));
        assert_eq!(
            run(&source),
            "DivisionByZero\n3\nsame error\n1\n",
            "{algorithm}"
        );
    }
}

#[test]
fn ordering_errors_propagate_from_search_and_array_selection() {
    assert_eq!(
        run(r#"
            use array;
            use ord::Ord;
            use search;
            struct Item { value: int }
            extend Item with Ord {
                fn compare(self, other: Self) -> int {
                    Error::MissingField("compare")
                }
            }
            fn report(action: int) {
                try {
                    if action == 0 { array::min([Item(value: 1), Item(value: 2)]); }
                    else if action == 1 { array::max([Item(value: 1), Item(value: 2)]); }
                    else { search::binary([Item(value: 1), Item(value: 2)], Item(value: 1)); }
                    crash("ordering error was ignored");
                } catch(error) {
                    when error is Error::MissingField(field) { io::println(field)!!; }
                    else { error!!; }
                }
            }
            fn main() { report(0); report(1); report(2); }
        "#),
        "compare\ncompare\ncompare\n"
    );
}

#[test]
fn generic_std_traits_support_typed_search_and_string_sorting() {
    assert_eq!(
        run(r#"
            use array;
            use str;
            fn main() {
                io::println(array::position([10, 20, 30], 20))!!;
                io::println(array::position(["red", "green", "blue"], "blue"))!!;
                io::println(array::position([nil, nil], nil))!!;
                var words = ["pear", "apple", "banana"];
                sort::quick(words);
                io::println(words)!!;
                var flags = [true, false, true, false];
                sort::quick(flags);
                io::println(flags)!!;
                io::println(str::compare("café", "caff"))!!;
            }
            "#),
        "1\n2\n0\n[apple, banana, pear]\n[false, false, true, true]\n1\n"
    );
}

#[test]
fn binary_search_and_bounds_support_duplicates_empty_arrays_and_strings() {
    assert_eq!(
        run(r#"
            use search;
            fn main() {
                const values = [1, 2, 2, 2, 5];
                io::println(search::binary(values, 2))!!;
                io::println(search::lower_bound(values, 2))!!;
                io::println(search::upper_bound(values, 2))!!;
                io::println(search::binary(values, -1))!!;
                io::println(search::lower_bound(values, -1))!!;
                io::println(search::upper_bound(values, 9))!!;

                const words = ["a", "c", "c", "z"];
                io::println(search::binary(words, "c"))!!;
                io::println(search::lower_bound(words, "c"))!!;
                io::println(search::upper_bound(words, "c"))!!;

                const empty: int[] = [];
                io::println(search::binary(empty, 3))!!;
                io::println(search::lower_bound(empty, 3))!!;
                io::println(search::upper_bound(empty, 3))!!;
            }
            "#),
        "2\n1\n4\n-1\n0\n5\n2\n1\n3\n-1\n0\n0\n"
    );
}

#[test]
fn array_min_and_max_use_generic_ordering_traits() {
    assert_eq!(
        run(r#"
            use array;
            fn main() {
                const values = [8, 3, 11, -2, 5];
                io::println(array::min(values))!!;
                io::println(array::max(values))!!;
            }
            "#),
        "-2\n11\n"
    );

    let (_, result) = execute(
        "use array; fn main() { const empty: int[] = []; array::min(empty)!!; }",
        VmCapabilities::default(),
    );
    assert!(
        result.unwrap_err().to_string().contains("array::min"),
        "empty arrays should report the operation in the error"
    );
}

#[test]
fn maps_use_hashing_preserve_order_and_handle_tombstones_and_errors() {
    let (output, result) = execute(
        r#"
            use collections;
            use clone;
            use eq::Eq;

            struct CollisionKey { value: int }
            extend CollisionKey with Eq[CollisionKey] {
                fn equals(self, other: CollisionKey) -> bool {
                    self.value == other.value
                }
            }
            extend CollisionKey with collections::Hashable {
                fn hash_code(self) -> int { -1 }
                fn equals(self, other: CollisionKey) -> bool {
                    self.value == other.value
                }
            }

            var assertion = 0;
            fn require(condition: bool) {
                assertion += 1;
                if !condition { crash($"map assertion failed at {assertion}"); }
            }

            var effects = 0;
            comptime OptionalInt: type = int?;
            fn failing_key() -> str { Error::MissingKey() }
            fn effect_value() -> int {
                effects += 1;
                99
            }

            fn main() {
                var numbers = collections::map_new[int, int]();
                require(numbers.is_empty());
                require(numbers.capacity() == 8);
                for index in 0..100 {
                    require(numbers.set(index, index * 3));
                }
                require(numbers.capacity() >= 128);
                require(numbers.len() == 100);
                require(numbers.get(73) == 219);
                require(!numbers.set(73, 220));
                require(numbers.get(73) == 220);
                require(numbers.get_or(900, -4) == -4);
                require(numbers.get_or(73, -4) == 220);

                for index in 0..80 {
                    const removed = numbers.remove(index);
                    if index == 73 { require(removed == 220); }
                    else { require(removed == index * 3); }
                }
                require(numbers.len() == 20);
                require(!numbers.contains(20 - 1));
                require(numbers.is_empty() == false);
                for index in 0..80 { require(numbers.set(index, index * 5)); }
                var expected = 80;
                for key in numbers.keys() {
                    require(key == expected);
                    expected += 1;
                    if expected == 100 { expected = 0; }
                }
                require(expected == 80);
                require(numbers.len() == 100);
                for index in 0..100 { numbers.remove(index); }
                require(numbers.len() == 0);
                require(numbers.is_empty());

                var collisions = collections::map_new[CollisionKey, str]();
                require(collisions.set(CollisionKey(value: 10), "ten"));
                require(collisions.set(CollisionKey(value: 20), "twenty"));
                require(collisions.set(CollisionKey(value: 30), "thirty"));
                require(collisions.get(CollisionKey(value: 20)) == "twenty");
                require(collisions.remove(CollisionKey(value: 20)) == "twenty");
                require(!collisions.contains(CollisionKey(value: 20)));
                require(collisions.get(CollisionKey(value: 30)) == "thirty");

                var words = collections::map_new[str, int]();
                require(words.set("é🦀", 0));
                require(words.contains("é🦀"));
                require(words.get("é🦀") == 0);
                require(words.set("other", 7));
                var snapshot = words.entries();
                snapshot << ("outside", 8);
                require(words.len() == 2);
                require(len(words.keys()) == 2);
                require(len(words.values()) == 2);
                var tuple_sum = 0;
                for entry in words.entries() { tuple_sum += entry.1; }
                require(tuple_sum == 7);

                const capacity = words.capacity();
                words.clear();
                require(words.is_empty());
                require(words.capacity() == capacity);
                require(words.set("reused", 9));
                require(words.get("reused") == 9);
                when words.get("missing") is Error::MissingKey {} else {
                    crash("get did not return MissingKey");
                }
                when words.remove("missing") is Error::MissingKey {} else {
                    crash("remove did not return MissingKey");
                }

                var flags = collections::map_new[bool, str]();
                require(flags.set(false, "off"));
                require(flags.set(true, "on"));
                require(flags.get(false) == "off");
                require(flags.get(true) == "on");

                var optional = collections::map_new[str, OptionalInt]();
                require(optional.set("nil", nil));
                require(optional.contains("nil"));
                require(optional.get("nil") == nil);

                var original = collections::map_new[str, int]();
                original.set("a", 1);
                original.set("b", 2);
                require(clone::value(original).get("a") == 1);
                var copied = original.clone();
                original.set("a", 10);
                copied.set("c", 3);
                require(copied.get("a") == 1);
                require(!original.contains("c"));
                require(copied.keys()[0]!! == "a");

                var many = collections::map_new[int, int]();
                for index in 0..100 { many.set(index, index); }
                const many_copy = many.clone();
                require(many_copy.len() == 100);
                require(many_copy.capacity() >= 128);
                expected = 0;
                for key in many_copy.keys() {
                    require(key == expected);
                    expected += 1;
                }

                try {
                    words.set(failing_key(), effect_value());
                    crash("map call did not preserve the argument Error");
                } catch(error) {
                    when error is Error::MissingKey {} else {
                        crash("map call replaced the argument Error");
                    }
                }
                require(effects == 0);
            }
        "#,
        VmCapabilities::default(),
    );
    assert!(result.is_ok(), "{output}\n{result:?}");
    assert!(output.is_empty());

    let private_field = project::compile_source(
        "private-map-field.cog",
        "include std; use collections; fn main() { const items = collections::map_new[str, int](); items.slots; }",
    )
    .unwrap_err()
    .to_string();
    assert!(private_field.contains("field `slots` is private"));

    for method in ["set(\"key\", 1)", "remove(\"key\")", "clear()"] {
        let source = format!(
            "include std; use collections; fn main() {{ const items = collections::map_new[str, int](); items.{method}; }}"
        );
        let diagnostic = project::compile_source("readonly-map-method.cog", &source)
            .unwrap_err()
            .to_string();
        assert!(
            diagnostic.contains("cannot mutate through const/read-only access"),
            "{method}: {diagnostic}"
        );
    }

    let unsupported_key = project::compile_source(
        "unsupported-map-key.cog",
        "include std; use collections; fn main() { collections::map_new[float, int](); }",
    )
    .unwrap_err()
    .to_string();
    assert!(
        unsupported_key.contains("does not implement trait `collections::Hashable`"),
        "{unsupported_key}"
    );

    let non_clone_value = project::compile_source(
        "non-clone-map-value.cog",
        "include std; use collections; struct NonClone { value: int } fn main() { const items = collections::map_new[int, NonClone](); items.clone(); }",
    )
    .unwrap_err()
    .to_string();
    assert!(
        non_clone_value.contains("does not implement trait `clone::Clone`"),
        "{non_clone_value}"
    );

    let private_constructor = project::compile_source(
        "private-map-constructor.cog",
        "include std; use collections; fn main() { collections::Map[str, int]([]); }",
    )
    .unwrap_err()
    .to_string();
    assert!(
        private_constructor
            .contains("cannot construct `collections::Map` outside its defining module")
    );
}

#[test]
fn collection_iteration_evaluates_once_uses_unicode_scalars_and_detects_structure_changes() {
    assert_eq!(
        run(r#"
            fn make_items() -> int[] {
                io::println("receiver")!!;
                [1, 2, 3]
            }

            fn main() {
                var total = 0;
                for item in make_items() { total += item; }
                io::println(total)!!;

                for character in "Aé🦀" { io::print(character)!!; }
                io::println("")!!;

                var values = [1, 2];
                try {
                    for value in values {
                        io::println(value)!!;
                        values << 3;
                    }
                    crash("array mutation did not invalidate iteration");
                } catch(error) {
                    io::println(error.error_kind())!!;
                }

                var replacements = [4, 5];
                for value in replacements {
                    replacements[0] = 9;
                    io::println(value)!!;
                }

                const empty: int[] = [];
                for unused in empty { crash("empty array should not iterate"); }

                for marker in [7, 8] {
                    defer { io::println(marker)!!; }
                    if marker == 7 { continue; }
                    break;
                }
            }
        "#),
        "receiver\n6\nAé🦀\n1\nInvalidArgument\n4\n5\n7\n8\n"
    );

    let not_iterable = project::compile_source(
        "not-iterable.cog",
        "include std; fn main() { for value in 42 { io::println(value); } }",
    )
    .unwrap_err()
    .to_string();
    assert!(not_iterable.contains("for-in requires an array or str"));

    let comptime_iteration = project::compile_source(
        "comptime-iteration.cog",
        "include std; comptime fn first(items: int[]) { for value in items { return value; } return 0; } fn main() {}",
    )
    .unwrap_err()
    .to_string();
    assert!(
        comptime_iteration.contains("collection iteration is available only in runtime functions")
    );
}

#[test]
fn bytes_copy_returns_independent_storage_and_obeys_buffer_limits() {
    assert_eq!(
        run(r#"
            use bytes;
            fn main() {
                var original = bytes::from_str("abc");
                const copied = bytes::copy(original);
                bytes::set(original, 0, 120);
                io::println(bytes::to_str(original, 0, bytes::len(original)))!!;
                io::println(bytes::to_str(copied, 0, bytes::len(copied)))!!;
            }
            "#),
        "xbc\nabc\n"
    );
}

#[test]
fn byte_slices_are_independent_and_text_writes_use_the_selected_stream() {
    assert_eq!(
        run(r#"
            use bytes;
            fn main() {
                var original = bytes::from_str("cog");
                const middle = bytes::slice(original, 1, 1);
                bytes::set(original, 1, 97);
                io::println(bytes::to_str(middle, 0, bytes::len(middle)))!!;
                io::write_text(io::stdout(), "done");
            }
            "#),
        "o\ndone"
    );
}

#[test]
fn byte_concatenation_handles_aliases_and_returns_independent_storage() {
    assert_eq!(
        run(r#"
            use bytes;
            fn main() {
                var left = bytes::from_str("ab");
                const repeated = bytes::concat(left, left);
                const joined = bytes::concat(left, bytes::from_str("cd"));
                bytes::set(left, 0, 120);
                io::println(bytes::to_str(repeated, 0, bytes::len(repeated)))!!;
                io::println(bytes::to_str(joined, 0, bytes::len(joined)))!!;
            }
            "#),
        "abab\nabcd\n"
    );
}

#[test]
fn error_kind_function_alias_and_method_inspect_without_triggering_catch() {
    assert_eq!(
        run(r#"
        use err;
        use err::kind as describe_error;
        fn main() {
            try {
                io::println(err::kind(1 / 0))!!;
                io::println(describe_error(1 / 0))!!;
                io::println((1 / 0).error_kind())!!;
                io::println(err::kind(7))!!;
                io::println(nil.error_kind())!!;
            } catch(error) { crash("inspection triggered catch"); }
        }
    "#),
        "DivisionByZero\nDivisionByZero\nDivisionByZero\nok\nok\n"
    );
}

#[test]
fn random_seeds_once_and_boolean_does_not_use_the_alternating_low_bit() {
    struct Fixture(std::path::PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root =
        std::env::temp_dir().join(format!("cog-stdlib-seed-{}-{unique}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let fixture = Fixture(root);
    // Compile the actual source with a deterministic entropy module in the same
    // test package. OS entropy is disabled, so accidental native use fails.
    std::fs::write(
        fixture.0.join("rng.cog"),
        include_str!("../core/random.cog")
            .replace("use entropy;", "use entropy_stub;")
            .replace("entropy::bytes", "entropy_stub::bytes"),
    )
    .unwrap();
    // This seed reaches zero on the current generator's first draw, exercising
    // the distinction between a valid zero state and an uninitialized generator.
    std::fs::write(
        fixture.0.join("entropy_stub.cog"),
        r#"
        use bytes as buffer;
        var reads = 0;
        public fn count() { reads }
        public fn bytes(count) {
            reads += 1;
            var result = buffer::new(4)!!;
            buffer::set(result, 0, 124)!!;
            buffer::set(result, 1, 119)!!;
            buffer::set(result, 2, 166)!!;
            buffer::set(result, 3, 131)!!;
            result
        }
    "#,
    )
    .unwrap();
    std::fs::write(
        fixture.0.join("main.cog"),
        r#"
        include std;
        use io; use rng; use entropy_stub;
        fn main() {
            for i in 0..16 { io::println(rng::boolean()!!)!!; }
            io::println(entropy_stub::count())!!;
        }
    "#,
    )
    .unwrap();
    let loaded = project::load(&fixture.0, None, false).unwrap();
    let program = project::compile(loaded.program, loaded.diagnostics, &loaded.names).unwrap();
    let program = bytecode::decode(&bytecode::encode(&program).unwrap()).unwrap();
    let mut output = Vec::new();
    interpreter::run_with_capabilities(
        &program,
        &mut output,
        VmCapabilities {
            allow_print: true,
            ..VmCapabilities::restricted()
        },
    )
    .unwrap();
    let output = String::from_utf8(output).unwrap();
    let lines = output.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 17);
    assert_eq!(lines[16], "1", "entropy was requested more than once");
    assert!(
        lines[..16]
            .iter()
            .all(|value| matches!(*value, "true" | "false"))
    );
    assert!(
        lines[..16].windows(2).any(|pair| pair[0] == pair[1]),
        "boolean output strictly alternated: {output}"
    );
}

#[test]
fn json_round_trips_dynamic_values_with_deterministic_object_order() {
    assert_eq!(
        run(r#"
            use json;
            fn main() {
                const value = json::parse("{\"z\":[true,null,-2,4.5,\"λ\"],\"a\":{\"x\":false}}")!!;
                io::println(json::stringify(value)!!)!!;
            }
        "#),
        "{\"a\":{\"x\":false},\"z\":[true,null,-2,4.5,\"λ\"]}\n"
    );
}

#[test]
fn json_errors_are_catchable_and_reject_duplicate_names() {
    let (output, result) = execute(
        r#"
            use json;
            use bytes;
            fn main() {
                when json::parse("{bad}") is Error::ParseFailure(from, to) {
                    io::println(from)!!;
                    io::println(to)!!;
                } else { crash("malformed JSON was accepted"); }
                when json::parse("{\"x\":1,\"x\":2}") is Error::ParseFailure(_, _) {
                    io::println("duplicate rejected")!!;
                } else { crash("duplicate keys were accepted"); }
                when json::parse("9223372036854775808") is Error::Overflow {
                    io::println("overflow")!!;
                } else { crash("out of range integer was accepted"); }
                when json::stringify(bytes::new(0)!!) is Error::TypeMismatch(op) {
                    io::println(op)!!;
                } else { crash("bytes unexpectedly serialized as JSON"); }
                when json::stringify(Error::MissingField("name")) is Error::MissingField(field) {
                    io::println(field)!!;
                } else { crash("top-level Error was not propagated"); }
            }
        "#,
        VmCapabilities::default(),
    );
    assert!(
        result.is_ok(),
        "partial output {output:?}, result: {result:?}"
    );
    assert_eq!(
        output,
        "JSON\nCog value\nduplicate rejected\noverflow\njson::stringify\nname\n"
    );
}

#[test]
fn json_decoded_arrays_remain_heterogeneous_and_mutable() {
    assert_eq!(
        run(r#"
            use json;
            fn main() {
                var value = json::parse("[1,\"two\",null]")!!;
                value << false;
                value[0] = "one";
                io::println(json::stringify(value)!!)!!;
            }
        "#),
        "[\"one\",\"two\",null,false]\n"
    );
}
