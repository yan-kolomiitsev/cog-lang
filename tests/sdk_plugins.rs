use cog::sdk::{Engine, PluginManifest, Signature, Type, Value};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);
struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "cog-sdk-Ω space-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn library(&self, name: &str) -> PathBuf {
        self.0.join(format!(
            "{}{name}{}",
            std::env::consts::DLL_PREFIX,
            std::env::consts::DLL_SUFFIX
        ))
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn check(command: &mut Command) {
    let result = command.output().unwrap();
    assert!(
        result.status.success(),
        "{command:?}\n{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn separately_compiled_rust_plugin_uses_c_abi_and_contains_panics() {
    let directory = Directory::new();
    let dependency_directory = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let library = std::fs::read_dir(&dependency_directory)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry.file_name().to_string_lossy().starts_with("libcog-")
                && entry.path().extension().is_some_and(|e| e == "rlib")
        })
        .max_by_key(|entry| entry.metadata().unwrap().modified().unwrap())
        .expect("compiled Cog library")
        .path();
    let output = directory.library("rust_plugin");
    let mut compiler = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()));
    compiler
        .arg("--edition=2024")
        .arg("--crate-type=cdylib")
        .arg("--crate-name=cog_sdk_fixture")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/native_plugin.rs"))
        .arg("--extern")
        .arg(format!("cog={}", library.display()))
        .arg("-L")
        .arg(format!("dependency={}", dependency_directory.display()))
        .arg("-o")
        .arg(&output);
    // Explicit-target Cargo builds keep host proc macros in a separate directory.
    let profile = dependency_directory.parent().unwrap().file_name().unwrap();
    for parent in dependency_directory.ancestors().take(4) {
        let profile_directory = parent.join(profile);
        let host_dependencies = profile_directory.join("deps");
        if host_dependencies.is_dir() {
            compiler
                .arg("-L")
                .arg(format!("dependency={}", host_dependencies.display()));
        }
        if let Ok(build_outputs) = std::fs::read_dir(profile_directory.join("build")) {
            for build in build_outputs.flatten() {
                let output = build.path().join("out");
                if output.is_dir() {
                    compiler
                        .arg("-L")
                        .arg(format!("native={}", output.display()));
                }
            }
        }
    }
    check(&mut compiler);
    let mut engine = Engine::new();
    unsafe { engine.load_plugin(&output) }.unwrap();
    let program = engine.compile("plugin.cog", "include std; use io; use host; use err; fn main() { io::println(host::double(21)); try { host::fail(); } catch(e) { io::println(e.error_kind()); } }").unwrap();
    let program = engine.decode(&program.encode().unwrap()).unwrap();
    drop(engine);
    let mut result = Vec::new();
    program.run(&mut result).unwrap();
    assert_eq!(result, b"42\nNativeFailure\n");
}

#[test]
fn plugin_manifests_are_strict_bounded_and_never_load_libraries() {
    let text = "plugin version=1 library=\"does-not-exist\"\nfunction \"host::double\" result=\"int\" { param \"int\"; }";
    let manifest = PluginManifest::parse(text).unwrap();
    assert!(PluginManifest::parse(&format!("// ignored \0\u{001D}\n{text}")).is_ok());
    let mut engine = Engine::new();
    manifest.register_metadata(&mut engine).unwrap();
    let program = engine
        .compile(
            "metadata.cog",
            "include std; use host; fn main() { host::double(21); }",
        )
        .unwrap();
    assert!(program.run(&mut Vec::new()).is_err());
    for invalid in [
        text.replace("version=1", "version=2"),
        text.replace("param \"int\"", "param \"unknown\""),
        text.replace("param \"int\"", "mutates 0"),
        format!("{text}\nplugin version=1 library=\"x\""),
        text.replace("param \"int\"", "effect \"output\"; effect \"output\""),
        format!("{text}\0"),
        " ".repeat(256 * 1024 + 1),
    ] {
        assert!(PluginManifest::parse(&invalid).is_err(), "{invalid:.100}");
    }
    let mut duplicate = Engine::new();
    duplicate
        .register("host::double", Signature::new(vec![], Type::Nil), |_, _| {
            Ok(Value::nil())
        })
        .unwrap();
    assert!(manifest.register_metadata(&mut duplicate).is_err());
}

#[test]
fn plugin_manifest_library_paths_must_be_relative_to_the_manifest() {
    let absolute = std::env::temp_dir().join("cog-plugin-library");
    let encoded = serde_json::to_string(absolute.to_str().unwrap()).unwrap();
    let text = format!("plugin version=1 library={encoded}");
    let error = PluginManifest::parse(&text).unwrap_err();
    assert!(error.to_string().contains("relative to the manifest"));

    // The library path is public and can be changed after parsing, so the
    // unsafe load boundary must enforce the same documented contract.
    let mut manifest = PluginManifest::parse("plugin version=1 library=\"plugin.so\"").unwrap();
    manifest.library = absolute;
    let error = unsafe { manifest.load(&mut Engine::new(), Path::new(".")) }.unwrap_err();
    assert!(error.to_string().contains("relative to the manifest"));
}

#[cfg(any(unix, windows))]
fn c_plugin(directory: &Directory, flags: &[&str]) -> PathBuf {
    let output = directory.library("c_plugin");
    let mut compiler = Command::new(if cfg!(windows) { "cl.exe" } else { "cc" });
    compiler.current_dir(&directory.0);
    let include = Path::new(env!("CARGO_MANIFEST_DIR")).join("include");
    if cfg!(windows) {
        compiler.args([
            "/nologo",
            "/LD",
            "/TC",
            "/std:c11",
            "/W4",
            "/WX",
            "/utf-8",
            "/D_CRT_SECURE_NO_WARNINGS",
        ]);
        for flag in flags {
            compiler.arg(format!(
                "/D{}",
                flag.strip_prefix("-D").expect("fixture define")
            ));
        }
        compiler.arg(format!("/I{}", include.display()));
        compiler.arg(format!("/Fe{}", output.display()));
        compiler.arg(format!(
            "/Fo{}",
            directory.0.join("native_plugin.obj").display()
        ));
    } else {
        compiler.args(["-std=c11", "-Wall", "-Wextra", "-Werror"]);
        if cfg!(target_os = "macos") {
            compiler.arg("-dynamiclib");
        } else {
            compiler.args(["-shared", "-fPIC"]);
        }
        compiler
            .args(flags)
            .arg("-I")
            .arg(include)
            .arg("-o")
            .arg(&output);
    }
    compiler.arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/native_plugin.c"));
    check(&mut compiler);
    output
}

#[cfg(any(unix, windows))]
#[test]
fn c_plugin_validates_handles_and_pins_state_and_library_until_program_drops() {
    let directory = Directory::new();
    let library = c_plugin(&directory, &[]);
    let marker = directory.0.join("destroyed");
    let mut engine = Engine::new();
    unsafe { engine.load_plugin(&library) }.unwrap();
    let source = format!(
        "include std; use io; use host; use err; fn main() {{ host::probe({}); io::println(host::probe(1)); try {{ host::probe(-1); }} catch(e) {{ io::println(e.error_kind()); }} io::println(host::probe(-2)); try {{ host::probe(0); }} catch(e) {{ io::println(e.error_kind()); }} try {{ host::probe(-3); }} catch(e) {{ io::println(e.error_kind()); }} }}",
        serde_json::to_string(marker.to_str().unwrap()).unwrap()
    );
    let program = engine
        .decode(&engine.compile("c.cog", &source).unwrap().encode().unwrap())
        .unwrap();
    drop(engine);
    assert!(!marker.exists());
    let mut output = Vec::new();
    program.run(&mut output).unwrap();
    assert_eq!(
        output,
        b"1\nNativeFailure\n2\nNativeFailure\nNativeFailure\n"
    );
    assert!(!marker.exists(), "program retains the library's state");
    drop(program);
    assert_eq!(std::fs::read(marker).unwrap(), b"destroyed");
}

#[cfg(any(unix, windows))]
#[test]
fn incompatible_descriptors_and_manifest_mismatch_leave_engine_unchanged() {
    for flag in [
        "-DTEST_PLUGIN_VERSION=2",
        "-DTEST_PLUGIN_SIZE=4",
        "-DTEST_FUNCTION_SIZE=4",
    ] {
        let directory = Directory::new();
        let library = c_plugin(&directory, &[flag]);
        assert!(unsafe { Engine::new().load_plugin(&library) }.is_err());
    }
    let directory = Directory::new();
    let library = c_plugin(&directory, &[]);
    let library_name = library.file_name().unwrap().to_str().unwrap();
    let manifest = PluginManifest::parse(&format!("plugin version=1 library={}\nfunction \"host::probe\" result=\"int\" {{ param \"any\"; effect \"fs_write\"; }}", serde_json::to_string(library_name).unwrap())).unwrap();
    let mut engine = Engine::new();
    assert!(unsafe { manifest.load(&mut engine, &directory.0) }.is_err());
    engine
        .register(
            "host::probe",
            Signature::new(vec![Type::Int], Type::Int),
            |_, args| Ok(args[0].clone()),
        )
        .unwrap();
}

#[cfg(any(unix, windows))]
#[test]
fn failed_plugin_create_destroys_partial_state_without_registering_functions() {
    let directory = Directory::new();
    let marker = directory.0.join("destroyed-after-create-failure");
    let marker_argument = format!(
        "-DTEST_CREATE_MARKER={}",
        serde_json::to_string(marker.to_str().unwrap()).unwrap()
    );
    let library = c_plugin(&directory, &["-DTEST_CREATE_FAILURE", &marker_argument]);
    let mut engine = Engine::new();

    let error = unsafe { engine.load_plugin(&library) }.unwrap_err();
    assert!(error.message().contains("plugin initialization failed"));
    assert_eq!(std::fs::read(marker).unwrap(), b"destroyed");
    assert!(
        engine
            .compile(
                "failed-plugin.cog",
                "include std; use host; fn main() { host::probe(1); }",
            )
            .is_err()
    );
}

#[test]
fn cli_check_and_build_use_metadata_without_loading_native_code() {
    let directory = Directory::new();
    let manifest = directory.0.join("plugin.kdl");
    let source = directory.0.join("main.cog");
    let output = directory.0.join("program.cogb");
    std::fs::write(&manifest, "plugin version=1 library=\"does-not-exist\"\nfunction \"host::double\" result=\"int\" { param \"int\"; }").unwrap();
    std::fs::write(
        &source,
        "include std; use host; fn main() { host::double(21); }",
    )
    .unwrap();
    check(
        Command::new(env!("CARGO_BIN_EXE_cog"))
            .arg("check")
            .arg(&source)
            .arg("--plugin")
            .arg(&manifest),
    );
    check(
        Command::new(env!("CARGO_BIN_EXE_cog"))
            .arg("build")
            .arg(&source)
            .arg("--plugin")
            .arg(&manifest)
            .arg("-o")
            .arg(&output),
    );
    assert!(output.is_file());
    let result = Command::new(env!("CARGO_BIN_EXE_cog"))
        .arg("run")
        .arg(&output)
        .arg("--plugin")
        .arg(&manifest)
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("does-not-exist"));
}

#[test]
fn retained_resource_values_do_not_revive_resources_in_later_contexts() {
    use std::cell::RefCell;
    use std::rc::Rc;
    let directory = Directory::new();
    let saved = Rc::new(RefCell::new(Value::nil()));
    let keep = saved.clone();
    let mut engine = Engine::new();
    engine
        .register(
            "host::keep",
            Signature::new(vec![Type::Handle], Type::Nil),
            move |_, arguments| {
                *keep.borrow_mut() = arguments[0].clone();
                Ok(Value::nil())
            },
        )
        .unwrap();
    engine
        .register(
            "host::restore",
            Signature::new(vec![], Type::Handle),
            move |_, _| Ok(saved.borrow().clone()),
        )
        .unwrap();
    let source = format!(
        "include std; use host; use io; fn main() {{ host::keep(io::open({}, \"w\", 0))!!; }}",
        serde_json::to_string(directory.0.join("file").to_str().unwrap()).unwrap()
    );
    engine
        .compile("open.cog", &source)
        .unwrap()
        .run(&mut Vec::new())
        .unwrap();
    let source = "include std; use host; use io; use err; fn main() { try { io::flush(host::restore()); } catch(e) { io::println(e.error_kind()); } }";
    let mut output = Vec::new();
    engine
        .compile("stale.cog", source)
        .unwrap()
        .run(&mut output)
        .unwrap();
    assert_eq!(output, b"InvalidResource\n");
}

#[test]
fn build_cache_includes_registered_native_metadata() {
    let directory = Directory::new();
    let source = directory.0.join("main.cog");
    let artifact = directory.0.join("main.cogb");
    std::fs::write(&source, "fn main() {}").unwrap();
    let first = Engine::new();
    assert!(
        !first
            .build_project(&source, &artifact, None, false)
            .unwrap()
            .reused
    );
    assert!(
        first
            .build_project(&source, &artifact, None, false)
            .unwrap()
            .reused
    );
    let mut second = Engine::new();
    second
        .register_metadata("host::added", Signature::new(vec![], Type::Nil))
        .unwrap();
    assert!(
        !second
            .build_project(&source, &artifact, None, false)
            .unwrap()
            .reused
    );
    assert!(
        second
            .build_project(&source, &artifact, None, false)
            .unwrap()
            .reused
    );
}

#[cfg(any(unix, windows))]
#[test]
fn cli_links_and_executes_explicit_plugins_from_source_and_bytecode() {
    let directory = Directory::new();
    let library = c_plugin(&directory, &[]);
    let manifest = directory.0.join("plugin.kdl");
    let source = directory.0.join("main.cog");
    let artifact = directory.0.join("program.cogb");
    let library_name = library.file_name().unwrap().to_str().unwrap();
    std::fs::write(&manifest, format!("plugin version=1 library={}\nfunction \"host::probe\" result=\"any\" {{ param \"any\"; effect \"fs_write\"; }}", serde_json::to_string(library_name).unwrap())).unwrap();
    std::fs::write(
        &source,
        "include std; use host; use io; fn main() { io::println(host::probe(1)); }",
    )
    .unwrap();
    check(
        Command::new(env!("CARGO_BIN_EXE_cog"))
            .arg("build")
            .arg(&source)
            .arg("--plugin")
            .arg(&manifest)
            .arg("-o")
            .arg(&artifact),
    );
    for path in [&source, &artifact] {
        let result = Command::new(env!("CARGO_BIN_EXE_cog"))
            .arg("run")
            .arg(path)
            .arg("--plugin")
            .arg(&manifest)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(result.stdout, b"1\n");
    }
    let result = Command::new(env!("CARGO_BIN_EXE_cog"))
        .current_dir(&directory.0)
        .args(["run", "program.cogb", "--plugin", "plugin.kdl"])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(result.stdout, b"1\n");
}
