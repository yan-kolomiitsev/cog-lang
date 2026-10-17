use cog::{bytecode, config, frontend, interpreter, lockfile, project};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "cog-project-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        Self(root)
    }
    fn write(&self, path: &str, text: &str) {
        let path = self.0.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let source = with_explicit_std_package(text);
        std::fs::write(path, source).unwrap();
    }
    fn write_raw(&self, path: &str, text: &str) {
        let path = self.0.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    fn cli(&self, command: &str, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_cog"))
            .arg(command)
            .arg(&self.0)
            .args(args)
            .output()
            .unwrap()
    }
    fn load(&self, locked: bool) -> Result<project::Loaded, String> {
        project::load(&self.0, None, locked).map_err(|errors| frontend::render(&errors))
    }
    fn compile(&self) -> Result<cog::backend::CompiledProgram, String> {
        let loaded = self.load(false)?;
        project::compile(loaded.program, loaded.diagnostics, &loaded.names)
    }
    fn run(&self) -> String {
        let compiled = self.compile().unwrap_or_else(|e| panic!("{e}"));
        let instructions = bytecode::decode(&bytecode::encode(&compiled).unwrap()).unwrap();
        let mut out = Vec::new();
        interpreter::run_with(&instructions, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }
    fn dependency(&self) {
        self.write("package.kdl", "package \"app\" version=\"0.1.0\"\n");
        self.write("dependencies.kdl", "dep path=\"packages/dep\"\n");
        self.write(
            "packages/dep/package.kdl",
            "package \"library\" version=\"0.2.0\"\n",
        );
        self.write(
            "packages/dep/src/lib.cog",
            "public fn answer() { return 42; }",
        );
    }
}

fn with_explicit_std_package(source: &str) -> String {
    const STANDARD_MODULES: &[&str] = &[
        "array", "ascii", "base64", "bytes", "clone", "entropy", "eq", "err", "fmt", "hex", "io",
        "math", "ord", "process", "random", "sort", "str", "time",
    ];

    let has_std_include = source.contains("include std;");
    let is_entry = source.contains("fn main");
    let source_body = source
        .strip_prefix("#!")
        .and_then(|rest| rest.split_once('\n').map(|(_, body)| body))
        .unwrap_or(source);
    let imports_std_module = source_body
        .split(';')
        .map(str::trim_start)
        .filter_map(|statement| statement.strip_prefix("use "))
        .any(|import| {
            if import.contains(" from ") {
                return false;
            }
            let module = import
                .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
                .next()
                .unwrap_or_default();
            STANDARD_MODULES.contains(&module)
        });

    if !is_entry || has_std_include || !imports_std_module {
        return source.to_owned();
    }

    if let Some((shebang, rest)) = source.split_once('\n').filter(|_| source.starts_with("#!")) {
        format!("{shebang}\ninclude std;\n{rest}")
    } else {
        format!("include std;\n{source}")
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn relative_path(from: &std::path::Path, to: &std::path::Path) -> PathBuf {
    let from = from.canonicalize().unwrap();
    let to = to.canonicalize().unwrap();
    let from_components: Vec<_> = from.components().collect();
    let to_components: Vec<_> = to.components().collect();
    let common = from_components
        .iter()
        .zip(&to_components)
        .take_while(|(left, right)| left == right)
        .count();
    let mut path = PathBuf::new();
    for _ in common..from_components.len() {
        path.push("..");
    }
    for component in &to_components[common..] {
        path.push(component.as_os_str());
    }
    path
}
fn success(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn cli_reports_version_without_a_command_or_path() {
    for argument in ["--version", "-V"] {
        let output = Command::new(env!("CARGO_BIN_EXE_cog"))
            .arg(argument)
            .output()
            .unwrap();
        success(&output);
        assert_eq!(
            output.stdout,
            format!("{} {}\n", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")).as_bytes()
        );
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn public_comptime_values_import_and_materialize_across_modules() {
    let f = Fixture::new();
    f.write("constants.cog", "public comptime BASE: int = 3;");
    f.write(
        "main.cog",
        "use constants; use io; comptime VALUE: int = constants::BASE * 2; fn main() { io::println(VALUE); }",
    );
    assert_eq!(f.run(), "6\n");
}

#[test]
fn public_comptime_functions_import_and_materialize_across_modules() {
    let f = Fixture::new();
    f.write(
        "constants.cog",
        "public comptime fn double(value: int) -> int { return value * 2; }",
    );
    f.write(
        "main.cog",
        "use constants; use io; comptime VALUE: int = constants::double(21); fn main() { io::println(VALUE); }",
    );
    assert_eq!(f.run(), "42\n");
}

#[cfg(unix)]
#[test]
fn executable_shebang_script_builds_caches_and_preserves_diagnostic_lines() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    f.write(
        "main.cog",
        "#!/usr/bin/env cog\nuse io; use helper; fn main() { io::println(helper::answer()); }\n",
    );
    f.write(
        "helper.cog",
        "#!/usr/bin/env cog\r\npublic fn answer() { return 42; }\n",
    );
    let script = f.0.join("main.cog");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let binary = std::path::Path::new(env!("CARGO_BIN_EXE_cog"));
    let mut paths = vec![binary.parent().unwrap().to_path_buf()];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let path = std::env::join_paths(paths).unwrap();
    for _ in 0..2 {
        let output = Command::new(&script).env("PATH", &path).output().unwrap();
        success(&output);
        assert_eq!(output.stdout, b"42\n");
    }
    success(&f.cli("check", &[]));
    success(&f.cli("build", &[]));
    let loaded = Command::new(binary)
        .arg(script.with_extension("cogb"))
        .output()
        .unwrap();
    success(&loaded);
    assert_eq!(loaded.stdout, b"42\n");
    f.write("main.cog", "#!/usr/bin/env cog\nfn main() { missing(); }\n");
    let failed = f.cli("check", &[]);
    assert!(!failed.status.success());
    let message = String::from_utf8(failed.stderr).unwrap();
    assert!(message.contains("main.cog:2:13"), "{message}");
    assert!(!message.contains("invalid token"), "{message}");
}

#[test]
fn formatting_imports_share_native_signatures_and_require_explicit_names() {
    let f = Fixture::new();
    for (imports, operations) in [
        (
            "use str; use fmt;",
            "var b = str::builder(); fmt::write(b, \"{}\", str::to_int(\"42\")); io::println(str::build(b));",
        ),
        (
            "use str::{builder, build, to_int}; use fmt::write;",
            "var b = builder(); write(b, \"{}\", to_int(\"42\")); io::println(build(b));",
        ),
        (
            "use str::*; use fmt::*;",
            "var b = builder(); write(b, \"{}\", to_int(\"42\")); io::println(build(b));",
        ),
    ] {
        f.write(
            "main.cog",
            &format!("use io; {imports} fn main() {{ {operations} }}"),
        );
        assert_eq!(f.run(), "42\n");
    }
    f.write("main.cog", "fn main() { to_str(1); to_int(\"42\"); }");
    let error = f.compile().unwrap_err();
    assert!(error.contains("unresolved name `to_str`"), "{error}");
    assert!(error.contains("unresolved name `to_int`"), "{error}");
}

#[test]
fn nominal_type_patterns_respect_aliases_modules_and_visibility() {
    let f = Fixture::new();
    f.write("model.cog", "public enum Event { Data(value: int), Empty } enum Hidden { Item } public fn item() { return Event::Data(7); }");
    f.write(
        "main.cog",
        r#"
        use io; use model; use model::Event as E; use err::Error as Failure;
        fn main() {
            const value = model::item();
            when value is E { io::println(true); }
            when value is E::Data { io::println(true); }
            when value is model::Event::Data { io::println(true); }
            when 1 / 0 is Failure::DivisionByZero { io::println(true); }
            when value is E::Data(payload) { io::println(payload); }
        }
    "#,
    );
    assert_eq!(f.run(), "true\ntrue\ntrue\ntrue\n7\n");
    f.write("main.cog", "use model; fn main() { const v = model::item(); when v is model::Hidden {} when v is Missing {} iserr(v); v.iserr(); }");
    let error = f.compile().unwrap_err();
    assert!(error.contains("private"), "{error}");
    assert!(error.contains("Missing"), "{error}");
    assert!(error.contains("unresolved name `iserr`"), "{error}");
    assert!(error.contains("unknown method `iserr()`"), "{error}");
}

#[test]
fn selective_core_constant_alias_works_in_interpolation() {
    let f = Fixture::new();
    f.write("package.kdl", "package \"app\" version=\"1\"\n");
    f.write(
        "main.cog",
        "use io; use math::PI as p; fn main() { io::println($\"{p}\"); }",
    );
    assert_eq!(f.run(), "3.141592653589793\n");
    success(&f.cli("build", &[]));
    let ran = Command::new(env!("CARGO_BIN_EXE_cog"))
        .arg("run")
        .arg(f.0.join("target/app.cogb"))
        .output()
        .unwrap();
    success(&ran);
    assert_eq!(
        String::from_utf8(ran.stdout).unwrap(),
        "3.141592653589793\n"
    );
}

#[test]
fn comptime_packages_include_sources_and_select_duplicate_module_names() {
    let f = Fixture::new();
    f.write("package.kdl", "package \"app\" version=\"0.1.0\"\n");
    f.write(
        "src/main.cog",
        "include game; include std; use io; use random from game as game_random; use random from std as std_random; fn main() { io::println(game_random::answer()); }",
    );
    f.write("src/game/random.cog", "public fn answer() -> int { 42 }");
    assert_eq!(f.run(), "42\n");
    assert!(!f.0.join("cog.lock").exists());
    success(&f.cli("build", &[]));
    assert!(f.0.join("cog.lock").exists());

    f.write(
        "src/main.cog",
        "include game; include std; use random; fn main() {}",
    );
    let error = f.compile().unwrap_err();
    assert!(error.contains("ambiguous module `random`"), "{error}");
    assert!(error.contains("select its package with `from`"), "{error}");
}

#[test]
fn block_imports_preserve_package_selection_and_aliases() {
    let f = Fixture::new();
    f.write("src/game/dice.cog", "public fn answer() -> int { 1 }");
    f.write("src/other/dice.cog", "public fn answer() -> int { 2 }");
    f.write_raw(
        "src/main.cog",
        r#"
        include std;
        include game;
        include other;
        use io;
        fn main() {
            use dice from game as first;
            use dice from other as second;
            use bytes from std as buffer;
            use math from std as numbers;
            io::println(first::answer());
            io::println(second::answer());
            io::println(buffer::len(buffer::new(3)));
            io::println(numbers::floor(1.9));
        }
        "#,
    );
    assert_eq!(f.run(), "1\n2\n3\n1\n");
}

#[test]
fn block_imports_reject_unknown_or_unconnected_packages() {
    let f = Fixture::new();
    f.write("src/marker.cog", "public fn answer() -> int { 99 }");
    f.write(
        "src/game/bytes.cog",
        "public fn new(size: int) -> int { 99 }",
    );
    for (source, expected) in [
        (
            "fn main() { use marker from absent; marker::answer(); }",
            "unknown module `marker` in package `absent`",
        ),
        (
            "fn main() { use bytes from game; bytes::new(1); }",
            "unknown module `bytes` in package `game`",
        ),
        (
            "include game; fn main() { use bytes from std; bytes::new(1); }",
            "unknown module `bytes` in package `std`",
        ),
    ] {
        f.write_raw("src/main.cog", source);
        let error = f.compile().unwrap_err();
        assert!(error.contains("error[E1003]"), "{error}");
        assert!(error.contains(expected), "{error}");
        assert!(error.contains("main.cog:1:"), "{error}");
    }
}

#[test]
fn nested_package_import_forms_resolve_a_unique_provider() {
    let f = Fixture::new();
    f.write("src/game/util/dice.cog", "public fn answer() -> int { 42 }");
    for (import, call) in [
        ("use util::dice;", "dice::answer()"),
        ("use util::dice::answer as pick;", "pick()"),
        ("use util::dice::{answer as pick};", "pick()"),
        ("use util::dice::*;", "answer()"),
    ] {
        for in_block in [false, true] {
            let (global, local) = if in_block { ("", import) } else { (import, "") };
            f.write_raw(
                "src/main.cog",
                &format!(
                    "include std; include game; use io; {global} fn main() {{ {local} io::println({call}); }}"
                ),
            );
            assert_eq!(f.run(), "42\n", "{import}, in_block={in_block}");
        }
    }
}

#[test]
fn nested_imports_reject_multiple_providers_including_local_modules() {
    for include_other in [false, true] {
        let f = Fixture::new();
        f.write("src/game/util/dice.cog", "public fn answer() -> int { 1 }");
        let includes = if include_other {
            f.write("src/other/util/dice.cog", "public fn answer() -> int { 2 }");
            "include game; include other;"
        } else {
            f.write("src/util/dice.cog", "public fn answer() -> int { 3 }");
            "include game;"
        };
        for import in [
            "use util::dice;",
            "use util::dice::answer;",
            "use util::dice::{answer};",
            "use util::dice::*;",
        ] {
            for in_block in [false, true] {
                let (global, local) = if in_block { ("", import) } else { (import, "") };
                f.write_raw(
                    "src/main.cog",
                    &format!("{includes} {global} fn main() {{ {local} }}"),
                );
                let error = f.compile().unwrap_err();
                assert!(error.contains("error[E1003]"), "{error}");
                assert!(error.contains("ambiguous module `util`"), "{error}");
                assert!(error.contains("select its package with `from`"), "{error}");
            }
        }
    }
}

#[test]
fn explicit_nested_import_paths_select_the_requested_provider() {
    let f = Fixture::new();
    f.write("src/game/util/dice.cog", "public fn answer() -> int { 1 }");
    f.write("src/other/util/dice.cog", "public fn answer() -> int { 2 }");
    f.write("src/util/dice.cog", "public fn answer() -> int { 3 }");
    for (import, expected) in [
        ("use util::dice::answer from game as pick;", "1\n"),
        ("use other::util::dice::answer as pick;", "2\n"),
        ("use super::util::dice::answer as pick;", "3\n"),
    ] {
        for in_block in [false, true] {
            let (global, local) = if in_block { ("", import) } else { (import, "") };
            f.write_raw(
                "src/main.cog",
                &format!(
                    "include std; include game; include other; use io; {global} fn main() {{ {local} io::println(pick()); }}"
                ),
            );
            assert_eq!(f.run(), expected, "{import}, in_block={in_block}");
        }
    }
}

#[test]
fn nested_package_imports_preserve_member_visibility() {
    let f = Fixture::new();
    f.write("src/game/util/dice.cog", "fn secret() -> int { 42 }");
    for (import, call, expected) in [
        ("use util::dice;", "dice::secret()", "`secret` is private"),
        ("use util::dice::secret;", "secret()", "`secret` is private"),
        (
            "use util::dice::{secret};",
            "secret()",
            "`secret` is private",
        ),
        ("use util::dice::*;", "secret()", "unresolved name `secret`"),
    ] {
        f.write_raw(
            "src/main.cog",
            &format!("include game; {import} fn main() {{ {call}; }}"),
        );
        let error = f.compile().unwrap_err();
        assert!(error.contains("error[E1003]"), "{error}");
        assert!(error.contains(expected), "{error}");
    }
}

#[test]
fn include_edges_participate_in_package_cycle_diagnostics() {
    for transitive in [false, true] {
        let f = Fixture::new();
        f.write("package.kdl", "package \"app\" version=\"1\"\n");
        f.write_raw("src/main.cog", "include game; fn main() {}");
        f.write("src/game/package.kdl", "package \"game\" version=\"1\"\n");
        let expected = if transitive {
            f.write("src/game/dependencies.kdl", "leaf path=\"../leaf\"\n");
            f.write("src/leaf/package.kdl", "package \"leaf\" version=\"1\"\n");
            f.write("src/leaf/dependencies.kdl", "app path=\"../..\"\n");
            "cyclic package dependency: app -> game -> leaf -> app"
        } else {
            f.write("src/game/dependencies.kdl", "app path=\"../..\"\n");
            "cyclic package dependency: app -> game -> app"
        };
        let output = f.cli("build", &[]);
        assert!(!output.status.success());
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(error.contains("error[E3001]"), "{error}");
        assert!(error.contains(expected), "{error}");
        assert!(error.contains("dependencies.kdl:1:1"), "{error}");
        assert!(!f.0.join("cog.lock").exists());
        assert!(!f.0.join("target/app.cogb").exists());
    }
}

#[test]
fn including_the_current_package_reports_a_located_cycle() {
    let f = Fixture::new();
    f.write("package.kdl", "package \"app\" version=\"1\"\n");
    let path = relative_path(&std::env::current_dir().unwrap(), &f.0)
        .to_string_lossy()
        .replace('\\', "/");
    f.write_raw(
        "src/main.cog",
        &format!("include app from {path:?}; fn main() {{}}"),
    );
    let error = f.compile().unwrap_err();
    assert!(error.contains("error[E3001]"), "{error}");
    assert!(
        error.contains("cyclic package dependency: app -> app"),
        "{error}"
    );
    assert!(error.contains("main.cog:1:1"), "{error}");
}

#[test]
fn included_packages_can_share_an_acyclic_dependency_and_a_stable_lockfile() {
    let f = Fixture::new();
    f.write("package.kdl", "package \"app\" version=\"1\"\n");
    for name in ["first", "second", "shared"] {
        f.write(
            &format!("src/{name}/package.kdl"),
            &format!("package {name:?} version=\"1\"\n"),
        );
    }
    f.write("src/shared/lib.cog", "public fn answer() -> int { 7 }");
    for name in ["first", "second"] {
        f.write(
            &format!("src/{name}/dependencies.kdl"),
            "shared path=\"../shared\"\n",
        );
        f.write(
            &format!("src/{name}/lib.cog"),
            "use shared; public fn answer() -> int { shared::answer() }",
        );
    }
    f.write_raw(
        "src/main.cog",
        "include std; include first; include second; use io; use first; use second; fn main() { io::println(first::answer() + second::answer()); }",
    );
    assert_eq!(f.run(), "14\n");
    success(&f.cli("build", &[]));
    let text = std::fs::read_to_string(f.0.join("cog.lock")).unwrap();
    assert!(text.starts_with("COGLOCK 1\n"));
    let lock = lockfile::decode("cog.lock", &text).unwrap();
    assert_eq!(lock.packages.len(), 4);
    assert_eq!(lock.dependencies.len(), 4);
    assert_eq!(lock.encode().unwrap(), text);
    success(&f.cli("build", &["--locked"]));
    assert_eq!(std::fs::read_to_string(f.0.join("cog.lock")).unwrap(), text);
}

#[test]
fn package_alias_conflicts_are_rejected_in_both_declaration_orders() {
    for alias in ["std", "game"] {
        for reverse in [false, true] {
            let f = Fixture::new();
            f.write("package.kdl", "package \"app\" version=\"1\"\n");
            f.write("packages/first/lib.cog", "");
            f.write("packages/second/lib.cog", "");
            let path = |name| {
                relative_path(&std::env::current_dir().unwrap(), &f.0.join(name))
                    .to_string_lossy()
                    .replace('\\', "/")
            };
            let first = format!("include {alias} from {:?};", path("packages/first"));
            let second = if alias == "std" {
                "include std;".to_owned()
            } else {
                format!("include {alias} from {:?};", path("packages/second"))
            };
            let (first, second) = if reverse {
                (second, first)
            } else {
                (first, second)
            };
            f.write_raw(
                "src/main.cog",
                &format!("{first}\n{second}\nfn main() {{}}"),
            );
            let output = f.cli("build", &[]);
            assert!(!output.status.success());
            let error = String::from_utf8(output.stderr).unwrap();
            assert!(error.contains("error[E3001]"), "{error}");
            assert!(
                error.contains(&format!(
                    "package alias `{alias}` selects more than one package"
                )),
                "{error}"
            );
            assert!(error.contains("main.cog:2:1"), "{error}");
            assert!(!f.0.join("cog.lock").exists());
            assert!(!f.0.join("target/app.cogb").exists());
        }
    }
}

#[test]
fn standard_include_cannot_replace_a_manifest_dependency_alias() {
    let f = Fixture::new();
    f.dependency();
    f.write("dependencies.kdl", "std path=\"packages/dep\"\n");
    f.write_raw("src/main.cog", "include std; fn main() {}");
    let output = f.cli("build", &[]);
    assert!(!output.status.success());
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(error.contains("error[E3001]"), "{error}");
    assert!(
        error.contains("package alias `std` selects more than one package"),
        "{error}"
    );
    assert!(error.contains("main.cog:1:1"), "{error}");
    assert!(!f.0.join("cog.lock").exists());
}

#[test]
fn repeated_includes_of_the_same_package_preserve_manifest_resolution() {
    let f = Fixture::new();
    f.dependency();
    f.write_raw(
        "src/main.cog",
        "include std; include dep; include std; include dep; use io; use dep; fn main() { io::println(dep::answer()); }",
    );
    assert_eq!(f.run(), "42\n");
    success(&f.cli("build", &[]));
    let text = std::fs::read_to_string(f.0.join("cog.lock")).unwrap();
    let lock = lockfile::decode("cog.lock", &text).unwrap();
    assert_eq!(lock.packages.len(), 2);
    assert_eq!(lock.dependencies.len(), 1);
    assert_eq!(lock.dependencies[0].1, "dep");
    success(&f.cli("build", &["--locked"]));
    assert_eq!(std::fs::read_to_string(f.0.join("cog.lock")).unwrap(), text);
}

#[test]
fn locked_builds_reject_existing_lockfiles_with_include_cycles() {
    let f = Fixture::new();
    f.write("package.kdl", "package \"app\" version=\"1\"\n");
    f.write("src/game/package.kdl", "package \"game\" version=\"1\"\n");
    f.write("src/game/dependencies.kdl", "app path=\"../..\"\n");
    f.write_raw("src/main.cog", "include game; fn main() {}");
    let text = "COGLOCK 1\nroot 0\npkg 0 \"app\" \"1\" path \".\"\npkg 1 \"game\" \"1\" path \"src/game\"\ndep 0 \"game\" 1\ndep 1 \"app\" 0\n";
    f.write_raw("cog.lock", text);
    let output = f.cli("build", &["--locked"]);
    assert!(!output.status.success());
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(error.contains("error[E3001]"), "{error}");
    assert!(
        error.contains("cyclic package dependency: app -> game -> app"),
        "{error}"
    );
    assert_eq!(std::fs::read_to_string(f.0.join("cog.lock")).unwrap(), text);
    assert!(!f.0.join("target/app.cogb").exists());
}

#[test]
fn cli_rejects_include_directives_outside_the_project_entry_point() {
    let f = Fixture::new();
    f.write("package.kdl", "package \"app\" version=\"1\"\n");
    f.write_raw(
        "src/main.cog",
        "use helper; fn main() { helper::answer(); }",
    );
    for include in [
        "include missing;",
        "include std;",
        "include missing from \"./definitely_absent\";",
    ] {
        f.write_raw(
            "src/helper.cog",
            &format!("// Unsupported context.\n{include}\npublic fn answer() -> int {{ 42 }}"),
        );
        for command in ["check", "build"] {
            let output = f.cli(command, &[]);
            assert!(!output.status.success());
            assert!(output.stdout.is_empty());
            let error = String::from_utf8(output.stderr).unwrap();
            assert!(error.contains("error[E3001]"), "{error}");
            assert!(
                error.contains("package inclusion is only supported in the project entry point"),
                "{error}"
            );
            assert!(error.contains("helper.cog:2:1"), "{error}");
            assert!(!f.0.join("cog.lock").exists());
            assert!(!f.0.join("target/app.cogb").exists());
        }
    }
}

#[test]
fn compile_source_rejects_source_package_includes_with_located_diagnostics() {
    for (include, package) in [
        ("include missing;", "missing"),
        ("include missing from \"./definitely_absent\";", "missing"),
        ("include std from \"./definitely_absent\";", "std"),
    ] {
        let source = format!("include std;\n{include}\nfn main() {{}}");
        let error = project::compile_source("unsupported-includes.cog", &source).unwrap_err();
        assert!(error.contains("error[E3001]"), "{error}");
        assert!(
            error.contains(&format!("unresolved package inclusion `{package}`")),
            "{error}"
        );
        assert!(
            error.contains("source packages require project::load or project::prepare"),
            "{error}"
        );
        assert!(error.contains("unsupported-includes.cog:2:1"), "{error}");
    }
}

#[test]
fn compile_source_preserves_embedded_standard_package_inclusion() {
    let program = project::compile_source(
        "embedded-standard.cog",
        "include std; include std; use io; use bytes from std; fn main() { io::println(bytes::len(bytes::new(4))); }",
    ).unwrap();
    let program = bytecode::decode(&bytecode::encode(&program).unwrap()).unwrap();
    let mut output = Vec::new();
    interpreter::run_with(&program, &mut output).unwrap();
    assert_eq!(output, b"4\n");
}

#[test]
fn root_standard_modules_require_include_in_cli_and_in_memory_compilation() {
    for import in [
        "use io;",
        "use io from std;",
        "use bytes;",
        "use bytes from std;",
        "use math;",
        "use math from std;",
    ] {
        let f = Fixture::new();
        let source = format!("{import}\nfn main() {{}}\n");
        // Do not use Fixture::write: that helper inserts the standard include.
        f.write_raw("main.cog", &source);
        let missing = f.cli("check", &[]);
        assert!(!missing.status.success(), "accepted {import} without std");
        let diagnostic = String::from_utf8(missing.stderr).unwrap();
        assert!(diagnostic.contains("error[E1003]"), "{diagnostic}");
        assert!(diagnostic.contains("main.cog:1:1"), "{diagnostic}");
        let diagnostic = project::compile_source("missing-std.cog", &source).unwrap_err();
        assert!(diagnostic.contains("error[E1003]"), "{diagnostic}");

        let connected = format!("include std;\n{source}");
        f.write_raw("main.cog", &connected);
        success(&f.cli("check", &[]));
        let compiled = project::compile_source("connected-std.cog", &connected).unwrap();
        let decoded = bytecode::decode(&bytecode::encode(&compiled).unwrap()).unwrap();
        interpreter::run_with_capabilities(
            &decoded,
            &mut Vec::new(),
            cog::runtime::VmCapabilities::restricted(),
        )
        .unwrap();
        assert!(!f.0.join("cog.lock").exists());
    }
}

#[test]
fn prepared_projects_reject_new_unresolved_include_declarations() {
    let f = Fixture::new();
    f.write_raw("main.cog", "include std; fn main() {}");
    let prepared = project::prepare(&f.0, None, false)
        .unwrap_or_else(|errors| panic!("{}", frontend::render(&errors)));
    f.write_raw("main.cog", "include std;\ninclude missing;\nfn main() {}");
    let loaded = prepared.resolve();
    let error = project::compile(loaded.program, loaded.diagnostics, &loaded.names).unwrap_err();
    assert!(error.contains("error[E3001]"), "{error}");
    assert!(
        error.contains("unresolved package inclusion `missing`"),
        "{error}"
    );
    assert!(error.contains("main.cog:2:1"), "{error}");
}

#[test]
fn standard_library_is_connected_as_an_explicit_comptime_package() {
    let f = Fixture::new();
    f.write("package.kdl", "package \"app\" version=\"0.1.0\"\n");

    f.write_raw("main.cog", "use io; fn main() { io::println(42); }");
    let error = f.compile().unwrap_err();
    assert!(error.contains("unknown import `io`"), "{error}");

    f.write_raw(
        "main.cog",
        "use bytes from std; fn main() { bytes::new(1); }",
    );
    let error = f.compile().unwrap_err();
    assert!(
        error.contains("unknown module `bytes` in package `std`"),
        "{error}"
    );

    f.write(
        "main.cog",
        "include std; use io; use bytes from std; fn main() { io::println(bytes::len(bytes::new(1))); }",
    );
    assert_eq!(f.run(), "1\n");
}

#[test]
fn comptime_package_import_without_collision_resolves_unique_module() {
    let f = Fixture::new();
    f.write("package.kdl", "package \"app\" version=\"0.1.0\"\n");
    f.write(
        "src/main.cog",
        "include game; include std; use io; use dice; fn main() { io::println(dice::answer()); }",
    );
    f.write("src/game/dice.cog", "public fn answer() -> int { 7 }");
    assert_eq!(f.run(), "7\n");
}

#[test]
fn comptime_package_paths_are_relative_to_the_invocation_directory() {
    let f = Fixture::new();
    f.write("package.kdl", "package \"app\" version=\"0.1.0\"\n");
    let external = f.0.join("external/game");
    f.write(
        "external/game/src/random.cog",
        "public fn answer() -> int { 9 }",
    );
    let relative = relative_path(&std::env::current_dir().unwrap(), &external)
        .to_string_lossy()
        .replace('\\', "/");
    f.write(
        "src/main.cog",
        &format!("include game from {relative}; use io; use random from game as game_random; fn main() {{ io::println(game_random::answer()); }}"),
    );
    assert_eq!(f.run(), "9\n");
}

#[test]
fn core_imports_keep_transitive_helpers_globals_and_own_qualified_calls() {
    let f = Fixture::new();
    f.write("main.cog", "use io; use math::{round, log2}; use sort::quick; fn main() { io::println(round(2.6)); io::println(log2(1)); var values = [3,1,2]; quick(values); io::println(values); }");
    assert_eq!(f.run(), "3\n0\n[1, 2, 3]\n");
}

#[cfg(unix)]
#[test]
fn process_example_builds_and_runs_through_cli_and_cached_artifact() {
    let f = Fixture::new();
    for source in [
        include_str!("../examples/processes.cog").to_owned(),
        include_str!("../examples/processes.cog")
            .replace("use process;", "use process::run;")
            .replace("process::run(", "run("),
        include_str!("../examples/processes.cog")
            .replace("use process;", "use process::*;")
            .replace("process::run(", "run("),
    ] {
        f.write("main.cog", &source);
        success(&f.cli("check", &[]));
        success(&f.cli("build", &[]));
        let run = f.cli("run", &[]);
        success(&run);
        assert_eq!(run.stdout, b"hello from child\n");
        assert!(run.stderr.is_empty());
    }
}

#[test]
fn process_args_and_environment_are_available_to_cli_programs() {
    let f = Fixture::new();
    let present = format!("COG_PROCESS_PRESENT_{}", std::process::id());
    let missing = format!("COG_PROCESS_MISSING_{}", std::process::id());
    f.write(
        "main.cog",
        &format!(
            r#"
            use io; use process;
            fn main() {{
                const args = process::args();
                if args.len() != 2 {{ crash("wrong argument count"); }}
                if args[0]!! != "a b" {{ crash("argument boundary was lost"); }}
                if args[1]!! != "é" {{ crash("Unicode argument was changed"); }}
                if process::env("{present}") != "ready" {{ crash("environment value missing"); }}
                if process::env("{missing}") != nil {{ crash("missing variable was not nil"); }}
                io::println("args-env-ok")!!;
            }}
            "#
        ),
    );

    let output = Command::new(env!("CARGO_BIN_EXE_cog"))
        .arg("run")
        .arg(&f.0)
        .arg("--")
        .arg("a b")
        .arg("é")
        .env(&present, "ready")
        .env_remove(&missing)
        .output()
        .unwrap();
    success(&output);
    assert_eq!(output.stdout, b"args-env-ok\n");
}

#[cfg(unix)]
#[test]
fn process_environment_rejects_non_utf8_values_as_runtime_errors() {
    use std::os::unix::ffi::OsStringExt;

    let f = Fixture::new();
    let key = format!("COG_PROCESS_NON_UTF8_{}", std::process::id());
    f.write(
        "main.cog",
        &format!(r#"use process; fn main() {{ process::env("{key}")!!; }}"#),
    );
    let output = Command::new(env!("CARGO_BIN_EXE_cog"))
        .arg("run")
        .arg(&f.0)
        .env(
            std::ffi::OsString::from(&key),
            std::ffi::OsString::from_vec(vec![0xff]),
        )
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cannot parse OS environment as str"),
        "{stderr}"
    );
}

#[test]
fn runtime_errors_include_locations_from_compiled_source_maps() {
    let f = Fixture::new();
    f.write("main.cog", "fn main() {\n    1 / 0\n}\n");
    let output = f.cli("run", &[]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("main.cog:2:5: division by zero"),
        "{stderr}"
    );
}

#[test]
fn restricted_cli_allows_output_and_denies_other_host_effects_for_source_and_artifacts() {
    let f = Fixture::new();
    f.write(
        "package.kdl",
        "package \"restricted-demo\" version=\"0.1.0\"\n",
    );
    f.write(
        "main.cog",
        "use io; fn main() { io::println(\"allowed\")!!; }",
    );
    let output = f.cli("run", &["--restricted"]);
    success(&output);
    assert_eq!(output.stdout, b"allowed\n");
    for source in [
        "use io; fn main() { io::open(\"must-not-exist\", \"w\", 0)!!; }",
        "use process; fn main() { const args: str[] = []; process::run(\"unused\", args)!!; }",
        "use time; fn main() { time::now()!!; }",
        "use entropy; fn main() { entropy::bytes(1)!!; }",
    ] {
        f.write("main.cog", source);
        let output = f.cli("run", &["--restricted"]);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("permission denied"));
        let output = Command::new(env!("CARGO_BIN_EXE_cog"))
            .args(["run", "--restricted"])
            .arg(f.0.join("target/restricted-demo.cogb"))
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("permission denied"));
    }
    assert!(!f.0.join("must-not-exist").exists());
}

#[test]
fn removed_process_aliases_are_compile_errors_and_native_modules_are_reserved() {
    let f = Fixture::new();
    for call in [
        "io::exec(\"unused\")",
        "io::Exec(\"unused\")",
        "exec(\"unused\")",
        "Exec(\"unused\")",
    ] {
        f.write("main.cog", &format!("use io; fn main() {{ {call}; }}"));
        assert!(f.compile().unwrap_err().contains("unresolved name"));
    }
    f.dependency();
    f.write("dependencies.kdl", "process path=\"packages/dep\"\n");
    f.write("main.cog", "fn main() { }");
    assert!(
        f.compile()
            .unwrap_err()
            .contains("standard/reserved module")
    );
}

#[test]
fn strict_imports_do_not_depend_on_dependency_declarations() {
    let f = Fixture::new();
    for with_dependency in [false, true] {
        if with_dependency {
            f.dependency();
        }
        for call in ["println(1)", "io::println(1)", "exec(\"echo unexpected\")"] {
            f.write("main.cog", &format!("fn main() {{ {call}; }}"));
            let error = f.compile().unwrap_err();
            assert!(error.contains("unresolved name"), "{error}");
        }
        f.write("main.cog", "use io::println; fn main() { println(1); }");
        assert_eq!(f.run(), "1\n");
    }
}

#[test]
fn init_creates_a_runnable_project_that_propagates_output_failures() {
    let f = Fixture::new();
    success(&f.cli("init", &[]));
    assert!(f.0.join("package.kdl").is_file());
    let name = f.0.file_name().unwrap().to_str().unwrap();
    assert_eq!(
        config::package(&f.0.join("package.kdl")).unwrap().name,
        name
    );
    success(&f.cli("check", &[]));
    success(&f.cli("build", &[]));
    assert!(f.0.join(format!("target/{name}.cogb")).is_file());
    let run = f.cli("run", &[]);
    success(&run);
    assert_eq!(String::from_utf8(run.stdout).unwrap(), "Hello, Cog!\n");
    let program = f.compile().unwrap();
    let program = bytecode::decode(&bytecode::encode(&program).unwrap()).unwrap();
    let mut output = Vec::new();
    let result = interpreter::run_with_capabilities(
        &program,
        &mut output,
        cog::runtime::VmCapabilities::restricted(),
    );
    assert_eq!(result.unwrap_err().kind(), "PermissionDenied");
    assert!(output.is_empty());
    assert!(!f.0.join("cog.lock").exists());
    assert!(!f.cli("init", &[]).status.success());
}

#[test]
fn init_requires_an_explicit_directory_without_modifying_the_current_directory() {
    for args in [vec!["init"], vec!["init", "--color", "never"]] {
        let f = Fixture::new();
        f.write_raw("keep.txt", "existing contents\n");
        let output = Command::new(env!("CARGO_BIN_EXE_cog"))
            .current_dir(&f.0)
            .args(args)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(error.contains("init requires a directory"), "{error}");
        let entries: Vec<_> = std::fs::read_dir(&f.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(entries, [std::ffi::OsString::from("keep.txt")]);
        assert_eq!(
            std::fs::read(f.0.join("keep.txt")).unwrap(),
            b"existing contents\n"
        );
    }
}

#[test]
fn init_uses_the_resolved_directory_name_for_explicit_relative_paths() {
    for path in [".", "nested/.."] {
        let f = Fixture::new();
        std::fs::create_dir(f.0.join("nested")).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_cog"));
        command.current_dir(&f.0).arg("init");
        command.arg(path);
        success(&command.output().unwrap());
        let package = config::package(&f.0.join("package.kdl")).unwrap();
        assert_eq!(package.name, f.0.file_name().unwrap().to_str().unwrap());
        success(&f.cli("check", &[]));
    }
}

#[test]
fn init_explains_kdl_configuration_and_creates_a_working_named_target() {
    let f = Fixture::new();
    success(&f.cli("init", &[]));
    let manifest = std::fs::read_to_string(f.0.join("package.kdl")).unwrap();
    let declarations: Vec<_> = manifest
        .lines()
        .filter(|line| line.starts_with("package "))
        .collect();
    assert_eq!(declarations.len(), 1);
    assert!(declarations[0].ends_with("version=\"0.1.0\""));
    assert!(manifest.contains("dependencies.kdl") && manifest.contains("build.kdl"));
    assert_eq!(
        config::package(&f.0.join("package.kdl")).unwrap().version,
        "0.1.0"
    );
    assert!(
        config::dependencies(&f.0.join("dependencies.kdl"))
            .unwrap()
            .is_empty()
    );
    let targets = config::targets(&f.0.join("build.kdl")).unwrap();
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].name, "app");
    assert_eq!(targets[0].entry, PathBuf::from("src/main.cog"));
    let run = f.cli("run", &["--target", "app"]);
    success(&run);
    assert_eq!(run.stdout, b"Hello, Cog!\n");
    assert!(f.0.join("target/app.cogb").is_file());
    assert!(!f.0.join("cog.lock").exists());

    // Activate the generated dependency example and use a real imported module.
    f.write_raw(
        "dependencies.kdl",
        "geometry path=\"./packages/geometry\"\n",
    );
    f.write_raw(
        "packages/geometry/package.kdl",
        "package \"geometry-library\" version=\"1.2.3\"\n",
    );
    f.write(
        "packages/geometry/shapes.cog",
        "public fn answer() -> int { 42 }",
    );
    f.write(
        "src/main.cog",
        "use io; use shapes from geometry; fn main() { io::println(shapes::answer())!!; }",
    );
    let run = f.cli("run", &["--target", "app"]);
    success(&run);
    assert_eq!(run.stdout, b"42\n");
    let lock = lockfile::decode(
        "cog.lock",
        &std::fs::read_to_string(f.0.join("cog.lock")).unwrap(),
    )
    .unwrap();
    assert!(
        lock.packages
            .iter()
            .any(|package| package.name == "geometry-library" && package.version == "1.2.3")
    );
}

#[test]
fn init_preserves_existing_dependency_and_build_configuration() {
    for name in ["dependencies.kdl", "build.kdl"] {
        let f = Fixture::new();
        f.write_raw(name, "// existing configuration\n");
        let result = f.cli("init", &[]);
        assert!(!result.status.success());
        assert_eq!(
            std::fs::read_to_string(f.0.join(name)).unwrap(),
            "// existing configuration\n"
        );
        assert!(!f.0.join("package.kdl").exists());
        assert!(!f.0.join("src").exists());
    }
}

#[test]
fn lockfile_write_preserves_existing_temporary_files() {
    let f = Fixture::new();
    let lock = f.0.join("cog.lock");
    let temporary = lock.with_extension(format!("lock.tmp.{}", std::process::id()));
    std::fs::write(&temporary, "existing temporary contents").unwrap();
    assert!(project::write_lock(Some((lock.clone(), "replacement".into()))).is_err());
    assert_eq!(
        std::fs::read_to_string(temporary).unwrap(),
        "existing temporary contents"
    );
    assert!(!lock.exists());
}

#[cfg(unix)]
#[test]
fn lockfile_write_does_not_follow_temporary_symlinks() {
    let f = Fixture::new();
    let lock = f.0.join("cog.lock");
    let temporary = lock.with_extension(format!("lock.tmp.{}", std::process::id()));
    let protected = f.0.join("protected.txt");
    std::fs::write(&protected, "preserved contents").unwrap();
    std::os::unix::fs::symlink(&protected, &temporary).unwrap();
    assert!(project::write_lock(Some((lock.clone(), "replacement".into()))).is_err());
    assert_eq!(
        std::fs::read_to_string(protected).unwrap(),
        "preserved contents"
    );
    assert!(
        temporary
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(!lock.exists());
}

#[cfg(unix)]
#[test]
fn init_serializes_directory_names_with_spaces_unicode_and_quotes_as_kdl() {
    let f = Fixture::new();
    let name = "project Ω \"quoted\"";
    let root = f.0.join(name);
    let initialized = Command::new(env!("CARGO_BIN_EXE_cog"))
        .arg("init")
        .arg(&root)
        .output()
        .unwrap();
    success(&initialized);
    assert_eq!(
        config::package(&root.join("package.kdl")).unwrap().name,
        name
    );
    let built = Command::new(env!("CARGO_BIN_EXE_cog"))
        .arg("build")
        .arg(&root)
        .output()
        .unwrap();
    success(&built);
    assert!(root.join(format!("target/{name}.cogb")).is_file());
}

#[test]
fn full_module_paths_distinguish_equal_basenames_and_ignore_unused_sources() {
    let f = Fixture::new();
    f.write("src/main.cog", "use io; use left::common as a; use right::common::{value as b}; fn main() { io::println(a::value()); io::println(b()); }");
    f.write("src/left/common.cog", "public fn value() { return 1; }");
    f.write("src/right/common.cog", "public fn value() { return 2; }");
    f.write(
        "src/unused.cog",
        "this is invalid source and is never imported",
    );
    assert_eq!(f.run(), "1\n2\n");
}

#[test]
fn imports_preserve_private_helpers_globals_and_defaults_at_declaration_site() {
    let f = Fixture::new();
    f.write("main.cog", "use io; use model::{get, Point}; fn main() { const BASE = 100; io::println(get()); io::println(Point().x); }");
    f.write("model.cog", "use str; const BASE = 9; fn helper() { return BASE; } public fn get(x = str::to_str(helper())) { return x; } public struct Point { x = helper() }");
    assert_eq!(f.run(), "9\n9\n");
}

#[test]
fn exported_functions_can_call_private_helpers_and_mutate_private_globals() {
    let f = Fixture::new();
    f.write(
        "counter.cog",
        r#"
var STATE = 0;
fn increment() { STATE = STATE + 1; return STATE; }
fn recurse(n) {
    if n == 0 { return increment(); }
    return counter::recurse(n - 1);
}

public fn next() { return counter::recurse(3); }
"#,
    );
    for imports in ["use counter;", "use counter::next;", "use counter::*;"] {
        let call = if imports == "use counter;" {
            "counter::next()"
        } else {
            "next()"
        };
        f.write(
            "main.cog",
            &format!("use io; {imports} fn main() {{ io::println({call}); io::println({call}); }}"),
        );
        assert_eq!(f.run(), "1\n2\n");
    }

    for (name, call) in [
        ("STATE", "STATE"),
        ("increment", "increment()"),
        ("recurse", "recurse(0)"),
    ] {
        for (imports, expression, diagnostic) in [
            (
                "use counter;".to_owned(),
                format!("counter::{call}"),
                "private",
            ),
            (format!("use counter::{name};"), call.to_owned(), "private"),
            (
                "use counter::*;".to_owned(),
                call.to_owned(),
                "unresolved name",
            ),
        ] {
            f.write(
                "main.cog",
                &format!("{imports} fn main() {{ {expression}; }}"),
            );
            let error = f.compile().unwrap_err();
            assert!(
                error.contains(diagnostic),
                "{imports} {expression}: {error}"
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn core_public_wrappers_keep_working_with_private_random_and_sort_helpers() {
    let f = Fixture::new();
    f.write(
        "main.cog",
        r#"
use io; use random::{next, range, boolean}; use sort::*;
fn main() {
    const n = next();
    io::println(n >= 0 && n < 2147483648);
    io::println(range(7, 8) == 7);
    const b = boolean();
    io::println(b == true || b == false);
    var items = [3, -1, 3, 0, 2];
    quick(items);
    io::println(items);
    var inserted = [2, 1]; insertion(inserted); io::println(inserted);
    var selected = [2, 1]; selection(selected); io::println(selected);
    var bubbled = [2, 1]; bubble(bubbled); io::println(bubbled);
    var empty: int[] = [];
    quick(empty); io::println(empty);
    var single = [1]; quick(single); io::println(single);
}
"#,
    );
    assert_eq!(
        f.run(),
        "true\ntrue\ntrue\n[-1, 0, 2, 3, 3]\n[1, 2]\n[1, 2]\n[1, 2]\n[]\n[1]\n"
    );
}

#[test]
fn core_implementation_helpers_are_private_for_all_import_forms() {
    let f = Fixture::new();
    for (module, name, call) in [
        ("array", "array_length", "array_length([], \"probe\")"),
        ("ascii", "byte_value", "byte_value(65, \"probe\")"),
        ("base64", "digit", "digit(10)"),
        ("base64", "sextet", "sextet(65)"),
        ("hex", "digit", "digit(10, false)"),
        ("hex", "nibble", "nibble(65)"),
        ("str", "bytes_match", "bytes_match(nil, 0, nil, 0)"),
        ("random", "state", "state"),
        ("random", "initialized", "initialized"),
        ("random", "MODULUS", "MODULUS"),
        ("random", "ensure_seeded", "ensure_seeded()"),
        ("random", "bytes_to_uint", "bytes_to_uint([1, 2])"),
        ("random", "range_bound", "range_bound(1)"),
        ("math", "number", "number(1, \"probe\")"),
        ("io", "output", "output(nil, nil)"),
        ("sort", "partition", "partition([2, 1], 0, 1)"),
        ("sort", "quick_sort_range", "quick_sort_range([2, 1], 0, 1)"),
    ] {
        for (imports, expression, diagnostic) in [
            (
                format!("use {module};"),
                format!("{module}::{call}"),
                "private",
            ),
            (format!("use {module}::{name};"), call.to_owned(), "private"),
            (
                format!("use {module}::*;"),
                call.to_owned(),
                "unresolved name",
            ),
        ] {
            f.write(
                "main.cog",
                &format!("{imports} fn main() {{ {expression}; }}"),
            );
            let error = f.compile().unwrap_err();
            assert!(
                error.contains(diagnostic),
                "{imports} {expression}: {error}"
            );
        }
    }
}

#[test]
fn native_calls_work_in_global_initializers() {
    let f = Fixture::new();
    f.write(
        "main.cog",
        "use io; use str; const S = str::to_str(42); fn main() { io::println(S); }",
    );
    assert_eq!(f.run(), "42\n");
}

#[test]
fn globals_initialize_after_dependencies_including_references_through_functions() {
    let f = Fixture::new();
    f.write(
        "main.cog",
        "use io; use values; const A = values::get(); fn main() { io::println(A); }",
    );
    f.write(
        "values.cog",
        "const B = C; const C = 7; public fn get() { return B; }",
    );
    assert_eq!(f.run(), "7\n");
}

#[test]
fn global_cycles_are_diagnosed_but_module_import_cycles_are_allowed() {
    let f = Fixture::new();
    f.write("main.cog", "use a; fn main() { a::f(); }");
    f.write("a.cog", "use b; public fn f() { return b::g(); }");
    f.write("b.cog", "use a; public fn g() { return 1; }");
    f.compile().unwrap();
    f.write(
        "a.cog",
        "use b; public const A = b::B; public fn f() { return A; }",
    );
    f.write("b.cog", "use a; public const B = a::A;");
    let error = f.compile().unwrap_err();
    assert!(error.contains("cyclic global initialization"), "{error}");
    assert!(error.contains("a::A") && error.contains("b::B"), "{error}");
}

#[test]
fn private_and_missing_imports_report_independent_errors_with_origins() {
    let f = Fixture::new();
    f.write(
        "main.cog",
        "use values::SECRET;\nuse missing;\nfn main() { const x: int = \"wrong\"; }\n",
    );
    f.write("values.cog", "const SECRET = 1;");
    let error = f.compile().unwrap_err();
    assert!(
        error.contains("private")
            && error.contains("unknown import")
            && error.contains("type mismatch"),
        "{error}"
    );
    assert!(
        error.contains("main.cog:1:1") && error.contains("main.cog:2:1"),
        "{error}"
    );
    assert!(!error.contains("$symbol"), "{error}");
}

#[test]
fn lexical_imports_shadow_outer_imports_without_leaking() {
    let f = Fixture::new();
    f.write("main.cog", "use io; use a::value; fn main() { io::println(value()); if true { use b::value; io::println(value()); } io::println(value()); }");
    f.write("a.cog", "public fn value() { return 1; }");
    f.write("b.cog", "public fn value() { return 2; }");
    assert_eq!(f.run(), "1\n2\n1\n");
}

#[test]
fn directory_namespaces_support_wildcard_and_super_imports() {
    let f = Fixture::new();
    f.write(
        "main.cog",
        "use io; use feature::worker::*; fn main() { io::println(value()); }",
    );
    f.write(
        "feature/worker.cog",
        "use super::base; public fn value() { return base::BASE; }",
    );
    f.write("feature/base.cog", "public const BASE = 6;");
    assert_eq!(f.run(), "6\n");
}

#[test]
fn imported_enums_keep_variant_identity() {
    let f = Fixture::new();
    f.write(
        "main.cog",
        "use io; use model::Result as R; fn main() { io::println(R::Ok(7)); }",
    );
    f.write("model.cog", "public enum Result { Ok(value), Empty }");
    assert!(f.run().contains('7'));
}

#[test]
fn transitive_packages_have_their_own_alias_scopes_and_preserve_package_boundaries() {
    let f = Fixture::new();
    f.dependency();
    f.write(
        "dependencies.kdl",
        "dep path=\"packages/dep\"\nleaf path=\"packages/other\"\n",
    );
    f.write("packages/dep/dependencies.kdl", "leaf path=\"../leaf\"\n");
    f.write(
        "packages/dep/src/lib.cog",
        "use leaf; public fn answer() { return leaf::value(); }",
    );
    for (path, value) in [("leaf", 11), ("other", 22)] {
        f.write(
            &format!("packages/{path}/package.kdl"),
            "package \"same-name\" version=\"1\"\n",
        );
        f.write(
            &format!("packages/{path}/src/lib.cog"),
            &format!("public fn value() {{ return {value}; }}"),
        );
    }
    f.write("main.cog", "use io; use dep; use leaf; fn main() { io::println(dep::answer()); io::println(leaf::value()); }");
    assert_eq!(f.run(), "11\n22\n");
    let lock = f.load(false).unwrap().lock_update.unwrap().1;
    assert_eq!(
        lockfile::decode("cog.lock", &lock).unwrap().packages.len(),
        4
    );
}

#[test]
fn declared_package_is_not_accessible_without_an_import() {
    let f = Fixture::new();
    f.dependency();
    f.write("main.cog", "fn main() { dep::answer(); }");
    assert!(f.compile().unwrap_err().contains("unresolved name"));
    f.write("main.cog", "use dep::answer; fn main() { answer(); }");
    f.compile().unwrap();
}

#[test]
fn nested_packages_are_not_accidentally_imported_as_local_modules() {
    let f = Fixture::new();
    f.dependency();
    f.write("main.cog", "use packages::dep; fn main() {}");
    assert!(f.compile().unwrap_err().contains("unknown item"));
}

#[test]
fn ambiguous_modules_and_package_cycles_are_errors() {
    let f = Fixture::new();
    f.write("main.cog", "fn main() {}");
    f.write("a.cog", "");
    f.write("a/b.cog", "");
    assert!(f.load(false).err().unwrap().contains("ambiguous module"));
    std::fs::remove_dir_all(f.0.join("a")).unwrap();
    f.dependency();
    f.write("packages/dep/dependencies.kdl", "app path=\"../..\"\n");
    assert!(
        f.load(false)
            .err()
            .unwrap()
            .contains("cyclic package dependency")
    );
}

#[test]
fn lockfile_is_stable_check_is_read_only_and_locked_detects_manifest_changes() {
    let f = Fixture::new();
    f.dependency();
    f.write(
        "main.cog",
        "use io; use dep; fn main() { io::println(dep::answer()); }",
    );
    assert!(!f.cli("build", &["--locked"]).status.success());
    success(&f.cli("check", &[]));
    assert!(!f.0.join("cog.lock").exists());
    assert!(!f.0.join("target").exists());
    success(&f.cli("build", &[]));
    let lock = std::fs::read(f.0.join("cog.lock")).unwrap();
    let artifact = std::fs::read(f.0.join("target/app.cogb")).unwrap();
    success(&f.cli("build", &["--locked"]));
    assert_eq!(std::fs::read(f.0.join("cog.lock")).unwrap(), lock);
    assert_eq!(
        std::fs::read(f.0.join("target/app.cogb")).unwrap(),
        artifact
    );
    f.write(
        "packages/dep/src/lib.cog",
        "public fn answer() { return 43; }",
    );
    let ran = f.cli("run", &["--locked"]);
    success(&ran);
    assert_eq!(String::from_utf8(ran.stdout).unwrap(), "43\n");
    f.write(
        "packages/dep/package.kdl",
        "package \"library\" version=\"0.3.0\"\n",
    );
    let stale = f.cli("build", &["--locked"]);
    assert!(!stale.status.success());
    assert!(String::from_utf8_lossy(&stale.stderr).contains("stale"));
    assert_eq!(std::fs::read(f.0.join("cog.lock")).unwrap(), lock);
    success(&f.cli("build", &[]));
    assert_ne!(std::fs::read(f.0.join("cog.lock")).unwrap(), lock);
}

#[test]
fn corrupt_lock_is_not_overwritten_and_failed_compilation_does_not_create_one() {
    let f = Fixture::new();
    f.dependency();
    f.write("main.cog", "fn main() { const x: int = \"wrong\"; }");
    assert!(!f.cli("build", &[]).status.success());
    assert!(!f.0.join("cog.lock").exists());
    assert!(!f.0.join("target/app.cogb").exists());
    f.write("main.cog", "fn main() {}");
    f.write("cog.lock", "COGLOCK 999\n");
    assert!(!f.cli("build", &[]).status.success());
    assert_eq!(
        std::fs::read_to_string(f.0.join("cog.lock")).unwrap(),
        "COGLOCK 999\n"
    );
}

#[test]
fn named_targets_have_separate_artifacts_and_require_explicit_choice() {
    let f = Fixture::new();
    f.write(
        "build.kdl",
        "target \"first\" entry=\"src/one.cog\"\ntarget \"second\" entry=\"src/two.cog\"\n",
    );
    f.write("src/one.cog", "use io; fn main() { io::println(1); }");
    f.write("src/two.cog", "use io; fn main() { io::println(2); }");
    assert!(!f.cli("build", &[]).status.success());
    success(&f.cli("build", &["--target", "first"]));
    success(&f.cli("build", &["--target", "second"]));
    assert!(f.0.join("target/first.cogb").is_file() && f.0.join("target/second.cogb").is_file());
    let run = f.cli("run", &["--target", "second"]);
    success(&run);
    assert_eq!(String::from_utf8(run.stdout).unwrap(), "2\n");
}

#[test]
fn kdl_schemas_reject_duplicates_unknown_fields_and_wrong_types_together() {
    let f = Fixture::new();
    f.write(
        "package.kdl",
        "package \"demo\" version=\"1\" version=\"2\" mystery=#true\nwrong 3\n",
    );
    let errors = config::package(&f.0.join("package.kdl")).unwrap_err();
    let rendered = frontend::render(&errors);
    assert!(
        rendered.contains("duplicate property")
            && rendered.contains("unknown property")
            && rendered.contains("expected a string")
            && rendered.contains("unknown node"),
        "{rendered}"
    );
    assert!(errors.iter().all(|e| e.span.is_some()));
    f.write(
        "package.kdl",
        "// comment\npackage #\"raw-name\"# version=\"1\"\n",
    );
    assert_eq!(
        config::package(&f.0.join("package.kdl")).unwrap().name,
        "raw-name"
    );
    f.write("dependencies.kdl", "lib path=\"one\"\nlib path=\"two\"\n");
    assert!(config::dependencies(&f.0.join("dependencies.kdl")).is_err());
    f.write("build.kdl", "target \"bad\" entry=\"../outside.cog\"\n");
    assert!(config::targets(&f.0.join("build.kdl")).is_err());
}

#[test]
fn lockfile_codec_is_canonical_and_handles_escaped_utf8() {
    let graph = lockfile::Lockfile {
        packages: vec![
            lockfile::Package {
                name: "root".into(),
                version: "1".into(),
                path: ".".into(),
            },
            lockfile::Package {
                name: "λ\"\\\n\r\t".into(),
                version: "2".into(),
                path: "packages/λ".into(),
            },
            lockfile::Package {
                name: "a".into(),
                version: "3".into(),
                path: "../a".into(),
            },
        ],
        dependencies: vec![(0, "z".into(), 1), (0, "a".into(), 2)],
    };
    let encoded = graph.encode().unwrap();
    assert_eq!(
        lockfile::decode("cog.lock", &encoded)
            .unwrap()
            .encode()
            .unwrap(),
        encoded
    );
    assert_eq!(
        lockfile::decode("cog.lock", &encoded.replace('\n', "\r\n"))
            .unwrap()
            .encode()
            .unwrap(),
        encoded
    );
    assert!(encoded.contains("pkg 1 \"a\" \"3\" path \"../a\""));
    assert!(!encoded.contains(&std::env::temp_dir().display().to_string()));
}

#[test]
fn malformed_lockfiles_never_panic_or_accept_broken_references() {
    let base = "COGLOCK 1\nroot 0\npkg 0 \"app\" \"1\" path \".\"\n";
    for bad in [
        "".to_string(),
        "COGLOCK 2\n".into(),
        format!("{base}dep 0 \"lib\" 999999999999999999999\n"),
        format!("{base}dep 0 \"lib\" 1\n"),
        format!("{base}pkg 2 \"lib\" \"1\" path \"lib\"\n"),
        format!("{base}root 0\n"),
        format!("{base}pkg 0 \"dup\" \"1\" path \"dup\"\n"),
        format!("{base}pkg 1 \"app\" \"1\" path \".\"\n"),
        format!("{base}dep 0 \"lib\" 0\ndep 0 \"lib\" 0\n"),
        format!("{base}pkg 01 \"lib\" \"1\" path \"lib\"\n"),
        format!("{base}pkg 1 \"lib\" \"1\" path \"/absolute\"\n"),
        format!("{base}pkg 1 \"lib\\q\" \"1\" path \"lib\"\n"),
        format!("{base}unknown \"value\"\n"),
    ] {
        assert!(
            lockfile::decode("cog.lock", &bad).is_err(),
            "accepted {bad}"
        );
    }
    assert!(
        lockfile::Lockfile {
            packages: vec![],
            dependencies: vec![]
        }
        .encode()
        .is_err()
    );
}

#[test]
fn independent_global_initializers_keep_declaration_order() {
    let f = Fixture::new();
    f.write("main.cog", "use io; fn mark(x) { io::println(x); return x; } const A = mark(1); const B = mark(2); const C = mark(3); const D = mark(4); const E = mark(5); const F = mark(6); const G = mark(7); const H = mark(8); const I = mark(9); const J = mark(10); const K = mark(11); fn main() {}");
    assert_eq!(f.run(), "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n");
}

#[test]
fn loading_package_root_does_not_implicitly_import_its_members() {
    let f = Fixture::new();
    f.write("main.cog", "use super; fn main() { value(); }");
    f.write("lib.cog", "public fn value() { return 7; }");
    assert!(f.compile().unwrap_err().contains("unresolved name `value`"));
    f.write(
        "main.cog",
        "use io; use super::value; fn main() { io::println(value()); }",
    );
    assert_eq!(f.run(), "7\n");
}

#[test]
fn project_build_produces_one_package_named_executable_with_all_linked_modules() {
    let f = Fixture::new();
    f.dependency();
    f.write("package.kdl", "package \"my-app\" version=\"1\"\n");
    f.write(
        "main.cog",
        "use io; use dep; use local; fn main() { io::println(dep::answer() + local::value()); }",
    );
    f.write("local.cog", "public fn value() { return 8; }");
    success(&f.cli("build", &[]));
    let artifacts: Vec<_> = std::fs::read_dir(f.0.join("target"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| {
            std::path::Path::new(name)
                .extension()
                .is_some_and(|ext| ext == "cogb")
        })
        .collect();
    assert_eq!(artifacts, vec![std::ffi::OsString::from("my-app.cogb")]);
    std::fs::remove_file(f.0.join("main.cog")).unwrap();
    std::fs::remove_file(f.0.join("local.cog")).unwrap();
    std::fs::remove_dir_all(f.0.join("packages")).unwrap();
    let ran = Command::new(env!("CARGO_BIN_EXE_cog"))
        .arg("run")
        .arg(f.0.join("target/my-app.cogb"))
        .output()
        .unwrap();
    success(&ran);
    assert_eq!(String::from_utf8(ran.stdout).unwrap(), "50\n");
}

#[test]
fn default_name_uses_directory_without_manifest_and_package_with_one_custom_entry() {
    let f = Fixture::new();
    f.write("main.cog", "fn main() {}");
    success(&f.cli("build", &[]));
    let name = f.0.file_name().unwrap().to_str().unwrap();
    assert!(f.0.join(format!("target/{name}.cogb")).is_file());
    std::fs::remove_file(f.0.join("main.cog")).unwrap();
    f.write("package.kdl", "package \"app\" version=\"1\"\n");
    f.write("build.kdl", "target \"worker\" entry=\"worker.cog\"\n");
    f.write("worker.cog", "fn main() {}");
    success(&f.cli("build", &[]));
    assert!(f.0.join("target/app.cogb").is_file());
    success(&f.cli("build", &["--target", "worker"]));
    assert!(f.0.join("target/worker.cogb").is_file());
}

#[test]
fn package_name_cannot_turn_the_default_output_into_an_arbitrary_path() {
    let f = Fixture::new();
    for name in ["../escape", "a/b", ".", "..", "a\\b", "a\nnew"] {
        // KDL uses the same escapes as Rust's debug representation here.
        f.write("package.kdl", &format!("package {name:?} version=\"1\"\n"));
        assert!(
            config::package(&f.0.join("package.kdl")).is_err(),
            "accepted {name:?}"
        );
    }
}

#[test]
fn run_builds_a_persistent_project_artifact_and_lock_before_execution() {
    let f = Fixture::new();
    f.dependency();
    f.write(
        "main.cog",
        "use io; use dep; fn main() { io::println(dep::answer()); }",
    );
    let ran = f.cli("run", &[]);
    success(&ran);
    assert_eq!(ran.stdout, b"42\n");
    let artifact = f.0.join("target/app.cogb");
    bytecode::decode(&std::fs::read(&artifact).unwrap()).unwrap();
    assert!(f.0.join("cog.lock").is_file());
    std::fs::remove_file(f.0.join("main.cog")).unwrap();
    std::fs::remove_dir_all(f.0.join("packages")).unwrap();
    let direct = Command::new(env!("CARGO_BIN_EXE_cog"))
        .arg("run")
        .arg(artifact)
        .output()
        .unwrap();
    success(&direct);
    assert_eq!(direct.stdout, ran.stdout);
    assert!(direct.stderr.is_empty());
}

#[test]
fn run_source_file_writes_and_rebuilds_the_sibling_artifact() {
    let f = Fixture::new();
    let source = f.0.join("hello.cog");
    let artifact = f.0.join("hello.cogb");
    for message in ["first", "updated"] {
        f.write(
            "hello.cog",
            &format!("use io; fn main() {{ io::println(\"{message}\"); }}"),
        );
        let ran = Command::new(env!("CARGO_BIN_EXE_cog"))
            .arg("run")
            .arg(&source)
            .output()
            .unwrap();
        success(&ran);
        assert_eq!(ran.stdout, format!("{message}\n").as_bytes());
        let direct = Command::new(env!("CARGO_BIN_EXE_cog"))
            .arg("run")
            .arg(&artifact)
            .output()
            .unwrap();
        success(&direct);
        assert_eq!(direct.stdout, ran.stdout);
    }
    assert!(!f.0.join("target/hello.cogb").exists());
    assert!(f.0.join("target/.cog-cache").is_dir());
}

#[test]
fn failed_recompilation_does_not_execute_or_overwrite_the_previous_artifact() {
    let f = Fixture::new();
    f.write("package.kdl", "package \"app\" version=\"1\"\n");
    f.write(
        "main.cog",
        "use io; fn main() { io::println(\"previous\"); }",
    );
    success(&f.cli("run", &[]));
    let artifact = f.0.join("target/app.cogb");
    let previous = std::fs::read(&artifact).unwrap();
    f.write(
        "main.cog",
        "use io; fn main() { const x: int = false; io::println(\"new\"); }",
    );
    let failed = f.cli("run", &[]);
    assert!(!failed.status.success());
    assert!(failed.stdout.is_empty());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("type mismatch"));
    assert_eq!(std::fs::read(&artifact).unwrap(), previous);
    let direct = Command::new(env!("CARGO_BIN_EXE_cog"))
        .arg("run")
        .arg(&artifact)
        .output()
        .unwrap();
    success(&direct);
    assert_eq!(direct.stdout, b"previous\n");
    assert_eq!(std::fs::read(artifact).unwrap(), previous);
}

#[test]
fn failed_artifact_write_prevents_execution_and_lock_update() {
    let f = Fixture::new();
    f.dependency();
    f.write(
        "main.cog",
        "use io; fn main() { io::println(\"must not execute\"); }",
    );
    std::fs::create_dir_all(f.0.join("target/app.cogb")).unwrap();
    let failed = f.cli("run", &[]);
    assert!(!failed.status.success());
    assert!(failed.stdout.is_empty());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("cannot write"));
    assert!(!f.0.join("cog.lock").exists());
    let entries: Vec<_> = std::fs::read_dir(f.0.join("target"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(entries, vec![std::ffi::OsString::from("app.cogb")]);
}

#[test]
fn runtime_failure_leaves_the_successfully_built_artifact() {
    let f = Fixture::new();
    f.write("package.kdl", "package \"app\" version=\"1\"\n");
    f.write("main.cog", "fn main() { crash(\"stop\", \"test\"); }");
    let failed = f.cli("run", &[]);
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("runtime error"));
    bytecode::decode(&std::fs::read(f.0.join("target/app.cogb")).unwrap()).unwrap();
}

#[test]
fn run_selects_sources_and_named_targets_independently_of_existing_artifacts() {
    let f = Fixture::new();
    f.write("package.kdl", "package \"app\" version=\"1\"\n");
    f.write(
        "build.kdl",
        "target \"client\" entry=\"client.cog\"\ntarget \"server\" entry=\"server.cog\"\n",
    );
    f.write("client.cog", "use io; fn main() { io::println(1); }");
    f.write("server.cog", "use io; fn main() { io::println(2); }");
    f.write("target/other.cogb", "unrelated invalid artifact");
    f.write("target/server.cogb", "outdated invalid artifact");
    let ambiguous = f.cli("run", &[]);
    assert!(!ambiguous.status.success());
    assert!(ambiguous.stdout.is_empty());
    assert!(String::from_utf8_lossy(&ambiguous.stderr).contains("no unambiguous entry point"));
    let selected = f.cli("run", &["--target", "server"]);
    success(&selected);
    assert_eq!(selected.stdout, b"2\n");
    bytecode::decode(&std::fs::read(f.0.join("target/server.cogb")).unwrap()).unwrap();
    assert!(!f.0.join("target/client.cogb").exists());
    f.write("main.cog", "use io; fn main() { io::println(3); }");
    let default = f.cli("run", &[]);
    success(&default);
    assert_eq!(default.stdout, b"3\n");
    assert!(f.0.join("target/app.cogb").is_file());
    assert_eq!(
        std::fs::read(f.0.join("target/other.cogb")).unwrap(),
        b"unrelated invalid artifact"
    );
}

#[test]
fn unchanged_build_and_run_share_an_artifact_without_rewriting_it() {
    let f = Fixture::new();
    f.dependency();
    f.write(
        "main.cog",
        "use io; use dep; fn main() { io::println(dep::answer()); }",
    );
    let built = f.cli("build", &[]);
    success(&built);
    assert!(
        String::from_utf8_lossy(&built.stderr)
            .lines()
            .any(|line| line.starts_with("built "))
    );
    let artifact = f.0.join("target/app.cogb");
    let modified = std::fs::metadata(&artifact).unwrap().modified().unwrap();
    let again = f.cli("build", &["--locked"]);
    success(&again);
    assert!(again.stderr.starts_with(b"finished "));
    let ran = f.cli("run", &["--locked"]);
    success(&ran);
    assert_eq!(ran.stdout, b"42\n");
    assert!(ran.stderr.is_empty());
    let debug = f.cli("run", &["--debug"]);
    success(&debug);
    assert!(debug.stderr.is_empty());
    assert_eq!(
        std::fs::metadata(&artifact).unwrap().modified().unwrap(),
        modified
    );
    let cache = std::fs::read_dir(f.0.join("target/.cog-cache"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let record = std::fs::read(&cache).unwrap();
    success(&f.cli("check", &[]));
    assert_eq!(std::fs::read(cache).unwrap(), record);
    assert_eq!(
        std::fs::metadata(artifact).unwrap().modified().unwrap(),
        modified
    );
}

#[test]
fn cli_reports_build_and_check_times_on_stderr_and_cached_run_is_quiet() {
    let f = Fixture::new();
    f.write("package.kdl", "package \"app\" version=\"1\"\n");
    f.write("main.cog", "use io; fn main() { io::println(42); }");
    for (action, label) in [
        ("build", "built"),
        ("build", "finished"),
        ("check", "checked"),
    ] {
        let output = f.cli(action, &["--color", "never"]);
        success(&output);
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.starts_with(label), "{stderr}");
        assert!(!stderr.contains("fresh"));
        let seconds: f64 = stderr
            .trim_end()
            .rsplit_once(" in ")
            .unwrap()
            .1
            .strip_suffix('s')
            .unwrap()
            .parse()
            .unwrap();
        assert!(seconds.is_finite() && seconds >= 0.0);
    }
    let ran = f.cli("run", &[]);
    success(&ran);
    assert_eq!(ran.stdout, b"42\n");
    assert!(ran.stderr.is_empty());
}

#[test]
fn cli_color_options_preserve_independent_errors_and_report_failure_count_and_time() {
    let f = Fixture::new();
    f.write(
        "main.cog",
        "fn main() { const a = ; const b: int = false; }",
    );
    for color in ["always", "never", "auto"] {
        let output = Command::new(env!("CARGO_BIN_EXE_cog"))
            .arg("build")
            .arg(&f.0)
            .args(["--color", color])
            .env("NO_COLOR", "1")
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert_eq!(stderr.contains("\x1b["), color == "always", "{stderr}");
        assert_eq!(stderr.matches("E0002").count(), 1, "{stderr}");
        assert_eq!(stderr.matches("E2001").count(), 1, "{stderr}");
        assert!(stderr.contains("main.cog:1:"), "{stderr}");
        assert!(
            stderr.contains("build failed for") && stderr.contains("due to 2 errors ("),
            "{stderr}"
        );
        if color == "always" {
            assert!(stderr.contains("\x1b[1;31merror[E0002]\x1b[0m"));
            assert!(stderr.contains("\x1b[1;36m-->\x1b[0m"));
        }
    }
    assert!(!f.0.join("target").exists());
}

#[test]
fn reported_build_time_excludes_program_execution() {
    let f = Fixture::new();
    f.write(
        "main.cog",
        "use io; use time; fn main() { time::sleep(0.2); io::println(42); }",
    );
    let started = std::time::Instant::now();
    let output = f.cli("run", &["--color", "never"]);
    let total = started.elapsed().as_secs_f64();
    success(&output);
    assert_eq!(output.stdout, b"42\n");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.starts_with("built "), "{stderr}");
    let reported: f64 = stderr
        .trim_end()
        .rsplit_once(" in ")
        .unwrap()
        .1
        .strip_suffix('s')
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        total - reported >= 0.19,
        "total={total}, reported={reported}"
    );
}

#[test]
fn source_content_changes_invalidate_cache_even_with_unchanged_mtime() {
    let f = Fixture::new();
    f.write("package.kdl", "package \"app\" version=\"1\"\n");
    f.write("main.cog", "use io; fn main() { io::println(1); }");
    success(&f.cli("build", &[]));
    let source = f.0.join("main.cog");
    let modified = std::fs::metadata(&source).unwrap().modified().unwrap();
    f.write("main.cog", "use io; fn main() { io::println(2); }");
    std::fs::File::options()
        .write(true)
        .open(&source)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(modified))
        .unwrap();
    assert_eq!(
        std::fs::metadata(source).unwrap().modified().unwrap(),
        modified
    );
    let ran = f.cli("run", &[]);
    success(&ran);
    assert!(ran.stderr.starts_with(b"built "));
    assert_eq!(ran.stdout, b"2\n");
}

#[test]
fn added_and_deleted_modules_invalidate_cached_import_resolution() {
    let f = Fixture::new();
    f.write(
        "main.cog",
        "use io; use math::PI; fn main() { io::println(PI); }",
    );
    let initial = f.cli("run", &[]);
    success(&initial);
    assert_eq!(initial.stdout, b"3.141592653589793\n");
    f.write("math.cog", "public const PI = 7;");
    let local = f.cli("run", &[]);
    assert!(!local.status.success());
    let diagnostic = String::from_utf8(local.stderr).unwrap();
    assert!(
        diagnostic.contains("ambiguous module `math`"),
        "{diagnostic}"
    );
    std::fs::remove_file(f.0.join("math.cog")).unwrap();
    let core = f.cli("run", &[]);
    success(&core);
    assert_eq!(core.stdout, initial.stdout);
}

#[test]
fn transitive_sources_manifests_and_dependency_paths_invalidate_cache() {
    let f = Fixture::new();
    f.dependency();
    f.write("packages/dep/dependencies.kdl", "leaf path=\"../leaf\"\n");
    f.write(
        "packages/dep/src/lib.cog",
        "use leaf; public fn answer() { return leaf::answer(); }",
    );
    f.write(
        "packages/leaf/package.kdl",
        "package \"leaf\" version=\"1\"\n",
    );
    f.write(
        "packages/leaf/src/lib.cog",
        "public fn answer() { return 1; }",
    );
    f.write(
        "main.cog",
        "use io; use dep; fn main() { io::println(dep::answer()); }",
    );
    success(&f.cli("build", &[]));
    f.write(
        "packages/leaf/src/lib.cog",
        "public fn answer() { return 2; }",
    );
    let source_changed = f.cli("run", &["--locked"]);
    success(&source_changed);
    assert!(source_changed.stderr.starts_with(b"built "));
    assert_eq!(source_changed.stdout, b"2\n");
    f.write(
        "packages/leaf/package.kdl",
        "package \"leaf\" version=\"2\"\n",
    );
    let locked = f.cli("run", &["--locked"]);
    assert!(!locked.status.success());
    assert!(locked.stdout.is_empty());
    assert!(String::from_utf8_lossy(&locked.stderr).contains("stale"));
    let manifest_changed = f.cli("run", &[]);
    success(&manifest_changed);
    assert!(
        String::from_utf8_lossy(&manifest_changed.stderr)
            .lines()
            .any(|line| line.starts_with("built "))
    );
    assert_eq!(manifest_changed.stdout, b"2\n");
    f.write(
        "packages/other/package.kdl",
        "package \"other\" version=\"1\"\n",
    );
    f.write(
        "packages/other/src/lib.cog",
        "public fn answer() { return 9; }",
    );
    f.write("dependencies.kdl", "dep path=\"packages/other\"\n");
    let path_changed = f.cli("run", &[]);
    success(&path_changed);
    assert!(
        String::from_utf8_lossy(&path_changed.stderr)
            .lines()
            .any(|line| line.starts_with("built "))
    );
    assert_eq!(path_changed.stdout, b"9\n");
}

#[test]
fn changing_a_target_entry_rebuilds_its_artifact() {
    let f = Fixture::new();
    f.write("build.kdl", "target \"worker\" entry=\"first.cog\"\n");
    f.write("first.cog", "use io; fn main() { io::println(1); }");
    f.write("second.cog", "use io; fn main() { io::println(2); }");
    success(&f.cli("build", &["--target", "worker"]));
    f.write("build.kdl", "target \"worker\" entry=\"second.cog\"\n");
    let ran = f.cli("run", &["--target", "worker"]);
    success(&ran);
    assert!(ran.stderr.starts_with(b"built "));
    assert_eq!(ran.stdout, b"2\n");
    assert!(f.0.join("target/worker.cogb").is_file());
}

#[test]
fn missing_or_modified_artifacts_and_cache_records_trigger_rebuilds() {
    let f = Fixture::new();
    f.write("package.kdl", "package \"app\" version=\"1\"\n");
    f.write("main.cog", "use io; fn main() { io::println(42); }");
    success(&f.cli("build", &[]));
    let artifact = f.0.join("target/app.cogb");
    let other = project::compile_source(
        "other.cog",
        "include std; use io; fn main() { io::println(99); }",
    )
    .unwrap();
    std::fs::write(&artifact, bytecode::encode(&other).unwrap()).unwrap();
    let replaced = f.cli("run", &[]);
    success(&replaced);
    assert!(replaced.stderr.starts_with(b"built "));
    assert_eq!(replaced.stdout, b"42\n");
    for damage in ["artifact", "record", "missing artifact", "missing target"] {
        match damage {
            "artifact" => std::fs::write(&artifact, b"COG").unwrap(),
            "record" => {
                let cache = std::fs::read_dir(f.0.join("target/.cog-cache"))
                    .unwrap()
                    .next()
                    .unwrap()
                    .unwrap()
                    .path();
                std::fs::write(cache, b"invalid cache metadata").unwrap();
            }
            "missing artifact" => std::fs::remove_file(&artifact).unwrap(),
            "missing target" => std::fs::remove_dir_all(f.0.join("target")).unwrap(),
            _ => unreachable!(),
        }
        let rebuilt = f.cli("run", &[]);
        success(&rebuilt);
        assert!(rebuilt.stderr.starts_with(b"built "), "{damage}");
        assert_eq!(rebuilt.stdout, b"42\n");
        let fresh = f.cli("run", &[]);
        success(&fresh);
        assert!(fresh.stderr.is_empty(), "{damage}");
    }
}

#[test]
fn a_warm_cache_does_not_bypass_lock_or_configuration_validation() {
    let f = Fixture::new();
    f.dependency();
    f.write(
        "main.cog",
        "use io; use dep; fn main() { io::println(dep::answer()); }",
    );
    success(&f.cli("build", &[]));
    let artifact = std::fs::read(f.0.join("target/app.cogb")).unwrap();
    let lock = std::fs::read(f.0.join("cog.lock")).unwrap();
    f.write("cog.lock", "broken lock");
    let corrupt = f.cli("run", &[]);
    assert!(!corrupt.status.success());
    assert!(corrupt.stdout.is_empty());
    assert!(String::from_utf8_lossy(&corrupt.stderr).contains("E3101"));
    assert_eq!(
        std::fs::read(f.0.join("target/app.cogb")).unwrap(),
        artifact
    );
    std::fs::remove_file(f.0.join("cog.lock")).unwrap();
    let locked = f.cli("run", &["--locked"]);
    assert!(!locked.status.success());
    assert!(locked.stdout.is_empty());
    let restored = f.cli("run", &[]);
    success(&restored);
    assert!(String::from_utf8_lossy(&restored.stderr).starts_with("updated "));
    assert!(!String::from_utf8_lossy(&restored.stderr).contains("built "));
    assert!(!String::from_utf8_lossy(&restored.stderr).contains("fresh"));
    assert_eq!(std::fs::read(f.0.join("cog.lock")).unwrap(), lock);
    f.write("package.kdl", "package \"app\" unexpected=\"invalid\"\n");
    let config_error = f.cli("run", &[]);
    assert!(!config_error.status.success());
    assert!(config_error.stdout.is_empty());
    assert!(String::from_utf8_lossy(&config_error.stderr).contains("E3003"));
}

#[test]
fn custom_output_and_default_output_have_separate_cache_records() {
    let f = Fixture::new();
    f.write("package.kdl", "package \"app\" version=\"1\"\n");
    f.write("main.cog", "use io; fn main() { io::println(42); }");
    // Keep discovery unchanged: adding an empty directory creates a namespace.
    f.write("out/.keep", "");
    success(&f.cli("build", &[]));
    let output = f.0.join("out/custom.cogb");
    let args = ["-o", output.to_str().unwrap()];
    let first = f.cli("build", &args);
    success(&first);
    assert!(first.stderr.starts_with(b"built "));
    let again = f.cli("build", &args);
    success(&again);
    assert!(again.stderr.starts_with(b"finished "));
    let default = f.cli("run", &[]);
    success(&default);
    assert!(default.stderr.is_empty());
    assert_eq!(default.stdout, b"42\n");
}

#[cfg(target_os = "linux")]
#[test]
fn compiler_contents_invalidate_cache_even_with_the_same_version_and_path() {
    use std::io::Write;
    fn run_alternate(binary: &std::path::Path, project: &std::path::Path) -> Output {
        // Concurrent forks can briefly retain a writable fd to the copied
        // binary. Retry only Linux ETXTBSY; compiler failures remain failures.
        for attempt in 0..100 {
            match Command::new(binary).arg("run").arg(project).output() {
                Err(error)
                    if error.kind() == std::io::ErrorKind::ExecutableFileBusy && attempt < 99 =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                result => return result.unwrap(),
            }
        }
        unreachable!()
    }
    let f = Fixture::new();
    f.write("main.cog", "use io; fn main() { io::println(42); }");
    success(&f.cli("build", &[]));
    let alternate = f.0.join("alternate-cog");
    std::fs::copy(env!("CARGO_BIN_EXE_cog"), &alternate).unwrap();
    let same = run_alternate(&alternate, &f.0);
    success(&same);
    assert!(same.stderr.is_empty());
    std::fs::OpenOptions::new()
        .append(true)
        .open(&alternate)
        .unwrap()
        .write_all(b"cog-compiler-identity-test")
        .unwrap();
    let changed = run_alternate(&alternate, &f.0);
    success(&changed);
    assert!(changed.stderr.starts_with(b"built "));
    assert_eq!(changed.stdout, b"42\n");
    let again = run_alternate(&alternate, &f.0);
    success(&again);
    assert!(again.stderr.is_empty());
}

#[test]
fn imported_generic_structs_keep_their_types_across_module_boundaries() {
    let f = Fixture::new();
    f.write(
        "src/model.cog",
        "public struct Box[T: type] { value: T }\
         public fn wrap[T: type](value: T) -> Box[T] { Box(value) }",
    );
    f.write(
        "src/main.cog",
        "use io; use model::Box; use model::wrap;\
         fn main() { const item: Box[int] = wrap(41); io::println(item.value + 1)!!; }",
    );
    assert_eq!(f.run(), "42\n");
}
