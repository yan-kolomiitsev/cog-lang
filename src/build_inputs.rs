//! Declared compiler inputs, contained by an explicitly selected directory.
use crate::diagnostic::SourceSpan;
use crate::error::RuntimeError;
use cap_std::fs::Dir;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;

const MAX_INPUT_BYTES: usize = 1_000_000;
const MAX_TOTAL_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const MAX_RECORD_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    Present(blake3::Hash),
    Absent,
    Unavailable,
}

#[derive(Debug, Default)]
struct Reads {
    states: BTreeMap<(PathBuf, String), State>,
    bytes: usize,
}

/// Captured compiler filesystem context. Clones share the dependency record.
#[derive(Debug, Clone)]
pub struct BuildInputs {
    base: PathBuf,
    sources: BTreeMap<String, PathBuf>,
    directories: BTreeMap<PathBuf, Rc<Dir>>,
    reads: Rc<RefCell<Reads>>,
}

impl BuildInputs {
    pub(crate) fn begin(&self) {
        *self.reads.borrow_mut() = Reads::default();
    }
    pub fn for_base(base: &Path) -> Result<Self, String> {
        let base = base
            .canonicalize()
            .map_err(|e| format!("cannot resolve build input base: {e}"))?;
        let directory = Dir::open_ambient_dir(&base, cap_std::ambient_authority())
            .map_err(|e| format!("cannot open build input base: {e}"))?;
        Ok(Self {
            directories: BTreeMap::from([(base.clone(), Rc::new(directory))]),
            base,
            sources: BTreeMap::new(),
            reads: Rc::new(RefCell::new(Reads::default())),
        })
    }

    pub fn from_current_dir() -> Result<Self, String> {
        Self::for_base(
            &std::env::current_dir()
                .map_err(|e| format!("cannot capture compiler working directory: {e}"))?,
        )
    }

    pub(crate) fn add_source(&mut self, name: String, base: &Path) -> Result<(), String> {
        let base = base
            .canonicalize()
            .map_err(|e| format!("cannot resolve package input base: {e}"))?;
        if !self.directories.contains_key(&base) {
            let directory = Dir::open_ambient_dir(&base, cap_std::ambient_authority())
                .map_err(|e| format!("cannot open package input base: {e}"))?;
            self.directories.insert(base.clone(), Rc::new(directory));
        }
        self.sources.insert(name, base);
        Ok(())
    }

    pub(crate) fn fingerprint(&self, hasher: &mut blake3::Hasher) {
        for path in std::iter::once(&self.base).chain(self.sources.values()) {
            let bytes = path.as_os_str().as_encoded_bytes();
            hasher.update(&(bytes.len() as u64).to_le_bytes());
            hasher.update(bytes);
        }
        for name in self.sources.keys() {
            hasher.update(&(name.len() as u64).to_le_bytes());
            hasher.update(name.as_bytes());
        }
    }

    pub(crate) fn read(
        &self,
        span: Option<&SourceSpan>,
        path: &str,
    ) -> Result<Result<Option<Vec<u8>>, RuntimeError>, String> {
        let base = span
            .and_then(|s| self.sources.get(&s.source.name))
            .unwrap_or(&self.base);
        self.read_at(base, path)
    }

    fn read_at(
        &self,
        base: &Path,
        path: &str,
    ) -> Result<Result<Option<Vec<u8>>, RuntimeError>, String> {
        let mut normalized = PathBuf::new();
        for component in Path::new(path).components() {
            match component {
                Component::Normal(part) => normalized.push(part),
                Component::CurDir => {}
                Component::ParentDir if normalized.pop() => {}
                _ => {
                    return Err(
                        "comptime build input path must be relative and cannot escape its base"
                            .into(),
                    );
                }
            }
        }
        if normalized.as_os_str().is_empty() {
            return Err("comptime build input path cannot be empty".into());
        }
        let name = normalized
            .to_str()
            .ok_or("build input path is not UTF-8")?
            .to_owned();
        let directory = self
            .directories
            .get(base)
            .ok_or("unregistered build input base")?;
        // Both metadata and open use the pinned capability. A symlink swap cannot
        // grant ambient filesystem authority between checking and opening.
        let result: io::Result<Vec<u8>> = (|| {
            if !directory.metadata(&normalized)?.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "build input must be a regular file",
                ));
            }
            let file = directory.open(&normalized)?;
            let mut bytes = Vec::new();
            file.take(MAX_INPUT_BYTES as u64 + 1)
                .read_to_end(&mut bytes)?;
            Ok(bytes)
        })();
        let (state, result) = match result {
            Ok(bytes) => {
                if bytes.len() > MAX_INPUT_BYTES {
                    return Err("compiler build input byte limit exceeded".into());
                }
                let mut reads = self.reads.borrow_mut();
                reads.bytes = reads
                    .bytes
                    .checked_add(bytes.len())
                    .ok_or("compiler build input byte limit exceeded")?;
                if reads.bytes > MAX_TOTAL_BYTES {
                    return Err("compiler total build input byte limit exceeded".into());
                }
                (State::Present(blake3::hash(&bytes)), Ok(Some(bytes)))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => (State::Absent, Ok(None)),
            Err(error)
                if error.kind() == io::ErrorKind::PermissionDenied
                    && error.to_string().contains("outside of the filesystem") =>
            {
                return Err("comptime build input symlink escapes its base".into());
            }
            Err(error) => (
                State::Unavailable,
                Err(RuntimeError::IoFailure {
                    op: "build input",
                    message: error.to_string(),
                }),
            ),
        };
        let mut reads = self.reads.borrow_mut();
        let key = (base.to_path_buf(), name);
        if let Some(previous) = reads.states.insert(key, state.clone())
            && previous != state
        {
            return Err("build input changed during compilation; retry the build".into());
        }
        if reads.states.len() > 4096 {
            return Err("compiler build input count limit exceeded".into());
        }
        Ok(result)
    }

    pub(crate) fn encode(&self) -> Result<Vec<u8>, String> {
        let mut document = kdl::KdlDocument::new();
        for ((base, path), state) in &self.reads.borrow().states {
            let mut node = kdl::KdlNode::new("input");
            node.insert(
                "base",
                base.to_str().ok_or("build input base is not UTF-8")?,
            );
            node.insert("path", path.clone());
            node.insert(
                "hash",
                match state {
                    State::Present(hash) => hash.to_hex().to_string(),
                    State::Absent => "absent".into(),
                    State::Unavailable => "unavailable".into(),
                },
            );
            document.nodes_mut().push(node);
        }
        let bytes = document.to_string().into_bytes();
        if bytes.len() > MAX_RECORD_BYTES {
            return Err("build input record limit exceeded".into());
        }
        Ok(bytes)
    }

    pub(crate) fn validate(&self, bytes: &[u8]) -> bool {
        if bytes.len() > MAX_RECORD_BYTES {
            return false;
        }
        let Ok(text) = std::str::from_utf8(bytes) else {
            return false;
        };
        if crate::limits::check_kdl_depth(text).is_err() {
            return false;
        }
        let Ok(document) = kdl::KdlDocument::parse_v2(text) else {
            return false;
        };
        if document.nodes().len() > 4096 {
            return false;
        }
        let fresh = Self {
            reads: Rc::new(RefCell::new(Reads::default())),
            ..self.clone()
        };
        let mut seen = std::collections::BTreeSet::new();
        for node in document.nodes() {
            if node.name().value() != "input"
                || node.children().is_some()
                || node.entries().len() != 3
            {
                return false;
            }
            let field = |name| node.get(name).and_then(|value| value.as_string());
            let (Some(base), Some(path), Some(hash)) =
                (field("base"), field("path"), field("hash"))
            else {
                return false;
            };
            let base = Path::new(base);
            if !self.directories.contains_key(base)
                || !seen.insert((base.to_owned(), path.to_owned()))
            {
                return false;
            }
            if fresh.read_at(base, path).is_err() {
                return false;
            }
            let reads = fresh.reads.borrow();
            let Some(state) = reads.states.get(&(base.to_owned(), path.to_owned())) else {
                return false;
            };
            if match state {
                State::Present(actual) => actual.to_hex().as_str() != hash,
                State::Absent => hash != "absent",
                State::Unavailable => true,
            } {
                return false;
            }
        }
        true
    }
}
