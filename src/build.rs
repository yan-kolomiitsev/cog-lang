//! Persistent executable builds with content-based freshness checks.
use crate::backend::CompiledProgram;
use crate::bytecode;
use crate::project::{self, Prepared};
use crate::resolver::Source;
use blake3::{Hash, Hasher};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"COGCACHE";
const VERSION: u16 = 2;
const RECORD_LEN: usize = 8 + 2 + 32 + 32;

pub struct Built {
    pub program: CompiledProgram,
    pub reused: bool,
}

/// Length-prefix each component so paths, names and contents cannot merge.
fn field(hasher: &mut Hasher, value: &[u8]) {
    hasher.update(&(value.len() as u64).to_le_bytes());
    hasher.update(value);
}

fn compiler_hash() -> Result<Hash, String> {
    let path = std::env::current_exe().map_err(|e| format!("cannot locate compiler: {e}"))?;
    let file = std::fs::File::open(&path)
        .map_err(|e| format!("cannot read compiler {}: {e}", path.display()))?;
    let mut hasher = Hasher::new();
    hasher
        .update_reader(file)
        .map_err(|e| format!("cannot hash compiler: {e}"))?;
    Ok(hasher.finalize())
}

/// Hash the discovered catalog as well as contents. Added/deleted modules and
/// package boundaries matter even when old import references did not name them.
/// Snapshot source bytes so compilation consumes exactly the hashed contents.
fn fingerprint(prepared: &mut Prepared, compiler: Hash) -> Result<Hash, String> {
    let mut hasher = Hasher::new();
    field(&mut hasher, b"cog-build-inputs-v1");
    crate::runtime::fingerprint_natives(&mut hasher);
    field(&mut hasher, compiler.as_bytes());
    field(&mut hasher, prepared.root.as_os_str().as_encoded_bytes());
    prepared.build_inputs.fingerprint(&mut hasher);
    field(&mut hasher, &(prepared.entry.0 as u64).to_le_bytes());
    field(&mut hasher, prepared.entry.1.as_bytes());
    field(&mut hasher, prepared.artifact_name.as_bytes());
    // Effective resolution avoids a gratuitous rebuild after creating cog.lock.
    // prepare() validates the actual lock, including --locked, on every call.
    field(&mut hasher, prepared.resolution.as_bytes());
    for ((owner, alias), target) in &prepared.aliases {
        field(&mut hasher, b"alias");
        field(&mut hasher, &(*owner as u64).to_le_bytes());
        field(&mut hasher, alias.as_bytes());
        field(&mut hasher, &(*target as u64).to_le_bytes());
    }
    for path in &prepared.manifests {
        field(&mut hasher, b"manifest");
        field(&mut hasher, path.as_os_str().as_encoded_bytes());
        match crate::limits::read_source(path) {
            Ok(bytes) => {
                field(&mut hasher, b"present");
                field(&mut hasher, &bytes);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => field(&mut hasher, b"absent"),
            Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
        }
    }
    let mut source_bytes = 0usize;
    for ((package, module), source) in &mut prepared.sources {
        field(&mut hasher, b"module");
        field(&mut hasher, &(*package as u64).to_le_bytes());
        field(&mut hasher, module.as_bytes());
        match source {
            Source::File(path) => {
                let path = path.clone();
                let bytes = crate::limits::read_source(&path)
                    .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
                source_bytes = charge_source_bytes(source_bytes, bytes.len())?;
                field(&mut hasher, b"file");
                field(&mut hasher, path.as_os_str().as_encoded_bytes());
                field(&mut hasher, &bytes);
                *source = Source::Snapshot(path, bytes);
            }
            Source::Snapshot(path, bytes) => {
                source_bytes = charge_source_bytes(source_bytes, bytes.len())?;
                field(&mut hasher, b"file");
                field(&mut hasher, path.as_os_str().as_encoded_bytes());
                field(&mut hasher, bytes);
            }
            Source::Embedded(name, text) => {
                source_bytes = charge_source_bytes(source_bytes, text.len())?;
                field(&mut hasher, b"embedded");
                field(&mut hasher, name.as_bytes());
                field(&mut hasher, text.as_bytes());
            }
            Source::Empty => field(&mut hasher, b"namespace"),
        }
    }
    Ok(hasher.finalize())
}

fn charge_source_bytes(current: usize, additional: usize) -> Result<usize, String> {
    current
        .checked_add(additional)
        .filter(|total| *total <= 64 * 1024 * 1024)
        .ok_or_else(|| "compiler total source byte limit (64 MiB) exceeded".to_owned())
}

fn cache_path(prepared: &Prepared, artifact: &Path) -> Result<PathBuf, String> {
    let absolute = std::path::absolute(artifact)
        .map_err(|e| format!("cannot resolve output {}: {e}", artifact.display()))?;
    let key = blake3::hash(absolute.as_os_str().as_encoded_bytes());
    Ok(prepared
        .root
        .join("target/.cog-cache")
        .join(key.to_hex().as_str()))
}

fn program(bytes: &[u8]) -> Result<CompiledProgram, String> {
    bytecode::decode(bytes)
}

fn reuse(cache: &Path, artifact: &Path, inputs: Hash) -> Option<CompiledProgram> {
    let file = std::fs::File::open(cache).ok()?;
    let mut record = Vec::new();
    file.take((RECORD_LEN + 1) as u64)
        .read_to_end(&mut record)
        .ok()?;
    if record.len() != RECORD_LEN
        || &record[..8] != MAGIC
        || record[8..10] != VERSION.to_le_bytes()
        || record[10..42] != *inputs.as_bytes()
    {
        return None;
    }
    let file = std::fs::File::open(artifact).ok()?;
    if file.metadata().ok()?.len() > bytecode::MAX_FILE_BYTES as u64 {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(bytecode::MAX_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > bytecode::MAX_FILE_BYTES {
        return None;
    }
    if record[42..] != *blake3::hash(&bytes).as_bytes() {
        return None;
    }
    program(&bytes).ok()
}

pub fn project(prepared: Prepared, artifact: &Path) -> Result<Built, String> {
    project_with_warnings(prepared, artifact).map(|(built, _)| built)
}

pub fn project_with_warnings(
    mut prepared: Prepared,
    artifact: &Path,
) -> Result<(Built, Vec<crate::diagnostic::Diagnostic>), String> {
    let cache = cache_path(&prepared, artifact)?;
    let inputs = fingerprint(&mut prepared, compiler_hash()?)?;
    let dependency_path = cache.with_extension("deps");
    let previous = dependency_record(&dependency_path);
    if let Some(previous) = previous
        && prepared.build_inputs.validate(&previous)
        && let Some(program) = reuse(&cache, artifact, with_dependencies(inputs, &previous))
    {
        project::write_lock(prepared.lock_update)?;
        return Ok((
            Built {
                program,
                reused: true,
            },
            Vec::new(),
        ));
    }
    let loaded = prepared.resolve();
    let (compiled, warnings) = project::compile_with_inputs_and_warnings(
        loaded.program,
        loaded.diagnostics,
        &loaded.names,
        &loaded.build_inputs,
    )?;
    let dependencies = loaded.build_inputs.encode()?;
    let inputs = with_dependencies(inputs, &dependencies);
    let bytes = bytecode::encode(&compiled)?;
    let program = program(&bytes)?;
    atomic_write(artifact, &bytes)?;
    project::write_lock(loaded.lock_update)?;
    let mut record = MAGIC.to_vec();
    record.extend_from_slice(&VERSION.to_le_bytes());
    record.extend_from_slice(inputs.as_bytes());
    record.extend_from_slice(blake3::hash(&bytes).as_bytes());
    atomic_write(&dependency_path, &dependencies)?;
    atomic_write(&cache, &record)?;
    // Execute these validated bytes even if another build later replaces the file.
    Ok((
        Built {
            program,
            reused: false,
        },
        warnings,
    ))
}

fn with_dependencies(inputs: Hash, dependencies: &[u8]) -> Hash {
    let mut hash = Hasher::new();
    field(&mut hash, b"cog-build-inputs-v2");
    field(&mut hash, inputs.as_bytes());
    field(&mut hash, dependencies);
    hash.finalize()
}

fn dependency_record(path: &Path) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(crate::build_inputs::MAX_RECORD_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() <= crate::build_inputs::MAX_RECORD_BYTES).then_some(bytes)
}

pub fn write_bytecode(path: &Path, compiled: &CompiledProgram) -> Result<(), String> {
    atomic_write(path, &bytecode::encode(compiled)?)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let temporary = path.with_extension(format!("tmp.{}", std::process::id()));
    let mut file = std::fs::File::create_new(&temporary)
        .map_err(|e| format!("cannot create {}: {e}", temporary.display()))?;
    let result = file.write_all(bytes);
    drop(file);
    let result = result.and_then(|()| std::fs::rename(&temporary, path));
    if let Err(e) = result {
        let _ = std::fs::remove_file(&temporary);
        return Err(format!("cannot write {}: {e}", path.display()));
    }
    Ok(())
}
