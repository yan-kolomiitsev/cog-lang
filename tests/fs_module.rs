use cog::runtime::{RuntimeContext, RuntimeLimits, VmCapabilities};
use cog::{bytecode, interpreter, project};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TempDirectory(PathBuf);

impl TempDirectory {
    fn new() -> Self {
        let serial = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("cog-fs-module-{}-{serial}", std::process::id()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn cog_string(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
            .replace('\t', "\\t")
    )
}

fn execute(source: &str, capabilities: VmCapabilities) {
    let source = format!(
        "include std; use fs; use err::Error;\nfn require(condition: bool) {{ if !condition {{ crash(\"assertion failed\"); }} }}\n{source}"
    );
    let program =
        project::compile_source("fs-module.cog", &source).unwrap_or_else(|error| panic!("{error}"));
    let program = bytecode::decode(&bytecode::encode(&program).unwrap()).unwrap();
    let mut sink = Vec::new();
    let mut context = RuntimeContext::new(&mut sink, capabilities);
    interpreter::run_with_context(&program, &mut context).unwrap();
    drop(context);
    assert!(sink.is_empty());
}

#[test]
fn filesystem_operations_cover_metadata_listing_mutation_and_os_errors() {
    let temp = TempDirectory::new();
    let file = temp.0.join("entry.txt");
    std::fs::write(&file, b"cog").unwrap();
    let created = temp.0.join("created");
    let nested = created.join("nested");
    let missing = temp.0.join("missing");

    execute(
        &format!(
            r#"
            fn main() {{
                const file = fs::metadata({})!!;
                require((file.is_file as bool) && !(file.is_dir as bool) && (file.len as int) == 3);
                const entries = fs::read_dir({})!!;
                require(len(entries) == 1 && entries[0] == "entry.txt");
                const directory = fs::metadata({})!!;
                require((directory.is_dir as bool) && !(directory.is_file as bool));

                fs::create_dir({})!!;
                fs::create_dir_all({})!!;
                require(fs::metadata({}).is_dir as bool);
                require(len(fs::read_dir({})!!) == 0);
                when fs::metadata({}) is Error::IoFailure(op, message) {{
                    require(op == "fs::metadata" && len(message) > 0);
                }} else {{ crash("missing path was accepted"); }}
                when fs::remove_dir({}) is Error::IoFailure(op, _) {{
                    require(op == "fs::remove_dir");
                }} else {{ crash("nonempty directory was removed"); }}

                fs::remove_file({})!!;
                fs::remove_dir({})!!;
                fs::remove_dir({})!!;
                fs::remove_dir({})!!;
            }}
            "#,
            cog_string(file.to_str().unwrap()),
            cog_string(temp.0.to_str().unwrap()),
            cog_string(temp.0.to_str().unwrap()),
            cog_string(created.to_str().unwrap()),
            cog_string(nested.to_str().unwrap()),
            cog_string(nested.to_str().unwrap()),
            cog_string(nested.to_str().unwrap()),
            cog_string(missing.to_str().unwrap()),
            cog_string(temp.0.to_str().unwrap()),
            cog_string(file.to_str().unwrap()),
            cog_string(nested.to_str().unwrap()),
            cog_string(created.to_str().unwrap()),
            cog_string(temp.0.to_str().unwrap()),
        ),
        VmCapabilities::default(),
    );
    assert!(!temp.0.exists());
}

#[test]
fn filesystem_permissions_are_checked_per_operation() {
    let temp = TempDirectory::new();
    execute(
        &format!(
            r#"
            fn main() {{
                when fs::metadata({}) is Error::PermissionDenied(op) {{
                    require(op == "fs::metadata");
                }} else {{ crash("filesystem read permission was bypassed"); }}
                when fs::read_dir({}) is Error::PermissionDenied(op) {{
                    require(op == "fs::read_dir");
                }} else {{ crash("directory-list permission was bypassed"); }}
                when fs::create_dir({}) is Error::PermissionDenied(op) {{
                    require(op == "fs::create_dir");
                }} else {{ crash("filesystem write permission was bypassed"); }}
            }}
            "#,
            cog_string(temp.0.to_str().unwrap()),
            cog_string(temp.0.to_str().unwrap()),
            cog_string(temp.0.join("denied").to_str().unwrap()),
        ),
        VmCapabilities::restricted(),
    );
    assert!(!temp.0.join("denied").exists());
}

#[test]
fn directory_listing_obeys_its_aggregate_buffer_limit() {
    let temp = TempDirectory::new();
    std::fs::write(temp.0.join("a"), b"").unwrap();
    let source = format!(
        r#"
        include std;
        use fs;
        use err::Error;
        fn main() {{
            when fs::read_dir({}) is Error::ResourceLimit(resource) {{
                if resource != "bytes" {{ crash("wrong resource limit"); }}
            }} else {{ crash("oversized listing was accepted"); }}
        }}
        "#,
        cog_string(temp.0.to_str().unwrap())
    );
    let program = project::compile_source("fs-list-limit.cog", &source)
        .unwrap_or_else(|error| panic!("{error}"));
    let program = bytecode::decode(&bytecode::encode(&program).unwrap()).unwrap();
    let mut sink = Vec::new();
    let mut context = RuntimeContext::new(&mut sink, VmCapabilities::default());
    context.limits = RuntimeLimits {
        max_buffer_bytes: Some(1),
        ..RuntimeLimits::default()
    };
    interpreter::run_with_context(&program, &mut context).unwrap();
    drop(context);
    assert!(sink.is_empty());
}
