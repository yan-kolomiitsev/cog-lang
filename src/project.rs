//! Filesystem discovery, local package resolution and build preparation.
use crate::ast::Program;
use crate::diagnostic::{Diagnostic, SourceSpan};
use crate::lockfile::{self, Lockfile};
use crate::resolver::{CORE, ModuleKey, Resolver, Source};
use crate::{config, frontend};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

pub struct Loaded {
    pub program: Program,
    pub diagnostics: Vec<Diagnostic>,
    pub lock_update: Option<(PathBuf, String)>,
    pub artifact_name: String,
    pub names: BTreeMap<String, String>,
    pub build_inputs: crate::build_inputs::BuildInputs,
}

impl Loaded {
    /// Compile a loaded project while retaining its captured build-input bases.
    pub fn compile(self) -> Result<crate::backend::CompiledProgram, String> {
        compile_with_inputs(
            self.program,
            self.diagnostics,
            &self.names,
            &self.build_inputs,
        )
    }
}

/// Discovery and configuration validation, including entry-point package declarations.
/// Preparation parses the entry point before the build cache is checked.
pub struct Prepared {
    pub artifact_name: String,
    pub lock_update: Option<(PathBuf, String)>,
    pub(crate) root: PathBuf,
    pub(crate) sources: BTreeMap<ModuleKey, Source>,
    pub(crate) aliases: BTreeMap<(usize, String), usize>,
    pub(crate) entry: ModuleKey,
    pub(crate) resolved_includes: BTreeSet<(String, Option<String>)>,
    pub(crate) manifests: Vec<PathBuf>,
    pub(crate) resolution: String,
    pub(crate) build_inputs: crate::build_inputs::BuildInputs,
}

impl Prepared {
    pub fn resolve(self) -> Loaded {
        let resolved = Resolver::new(self.sources, self.aliases, self.entry)
            .with_resolved_includes(self.resolved_includes)
            .resolve();
        Loaded {
            program: resolved.program,
            diagnostics: resolved.diagnostics,
            lock_update: self.lock_update,
            artifact_name: self.artifact_name,
            names: resolved.names,
            build_inputs: self.build_inputs,
        }
    }
}
struct Package {
    root: PathBuf,
    metadata: config::Package,
}
struct Graph {
    packages: Vec<Package>,
    by_path: BTreeMap<PathBuf, usize>,
    aliases: BTreeMap<(usize, String), usize>,
    alias_spans: BTreeMap<(usize, String), SourceSpan>,
    errors: Vec<Diagnostic>,
}

fn error(message: impl Into<String>) -> Diagnostic {
    Diagnostic::new("E3001", message, None)
}

impl Graph {
    fn bind_alias(&mut self, owner: usize, alias: &str, target: usize, span: Option<SourceSpan>) {
        let key = (owner, alias.to_owned());
        if let Some(previous) = self.aliases.get(&key) {
            if *previous != target {
                self.errors.push(Diagnostic::new(
                    "E3001",
                    format!("package alias `{alias}` selects more than one package"),
                    span,
                ));
            }
            return;
        }
        self.aliases.insert(key.clone(), target);
        if let Some(span) = span {
            self.alias_spans.insert(key, span);
        }
    }

    fn validate_cycles(&mut self) {
        fn visit(
            package: usize,
            packages: &[Package],
            edges: &[Vec<(usize, Option<SourceSpan>)>],
            states: &mut [u8],
            path: &mut Vec<usize>,
            errors: &mut Vec<Diagnostic>,
        ) {
            states[package] = 1;
            path.push(package);
            for (target, span) in &edges[package] {
                match states[*target] {
                    0 => visit(*target, packages, edges, states, path, errors),
                    1 => {
                        let start = path.iter().position(|id| id == target).unwrap();
                        let cycle = path[start..]
                            .iter()
                            .chain(std::iter::once(target))
                            .map(|id| packages[*id].metadata.name.as_str())
                            .collect::<Vec<_>>()
                            .join(" -> ");
                        errors.push(Diagnostic::new(
                            "E3001",
                            format!("cyclic package dependency: {cycle}"),
                            span.clone(),
                        ));
                    }
                    _ => {}
                }
            }
            path.pop();
            states[package] = 2;
        }

        let mut edges = vec![Vec::new(); self.packages.len()];
        for (key, target) in &self.aliases {
            if *target != CORE {
                edges[key.0].push((*target, self.alias_spans.get(key).cloned()));
            }
        }
        let mut states = vec![0; self.packages.len()];
        let mut path = Vec::new();
        for package in 0..self.packages.len() {
            if states[package] == 0 {
                visit(
                    package,
                    &self.packages,
                    &edges,
                    &mut states,
                    &mut path,
                    &mut self.errors,
                );
            }
        }
    }

    fn package(&mut self, root: &Path, required: bool) -> Option<usize> {
        let root = match root.canonicalize() {
            Ok(p) if p.is_dir() => p,
            _ => {
                self.errors.push(error(format!(
                    "package directory does not exist: {}",
                    root.display()
                )));
                return None;
            }
        };
        if root.to_str().is_none() {
            self.errors.push(error("package paths must be valid UTF-8"));
            return None;
        }
        if let Some(id) = self.by_path.get(&root) {
            return Some(*id);
        }
        if self.packages.len() >= 256 {
            self.errors
                .push(error("compiler package limit (256) exceeded"));
            return None;
        }
        let metadata = if root.join("package.kdl").exists() {
            match config::package(&root.join("package.kdl")) {
                Ok(p) => p,
                Err(errors) => {
                    self.errors.extend(errors);
                    return None;
                }
            }
        } else if required {
            self.errors.push(error(format!(
                "local dependency {} has no package.kdl",
                root.display()
            )));
            return None;
        } else {
            config::Package {
                name: root
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("app")
                    .into(),
                version: "0.0.0".into(),
            }
        };
        let id = self.packages.len();
        self.packages.push(Package {
            root: root.clone(),
            metadata,
        });
        self.by_path.insert(root.clone(), id);
        let deps = match config::dependencies(&root.join("dependencies.kdl")) {
            Ok(deps) => deps,
            Err(errors) => {
                self.errors.extend(errors);
                Vec::new()
            }
        };
        for dep in deps {
            if let Some(target) = self.package(&root.join(&dep.path), true) {
                self.bind_alias(id, &dep.alias, target, Some(dep.span.clone()));
                if matches!(dep.alias.as_str(), "self" | "super")
                    || crate::builtins::native_module(&dep.alias)
                    || crate::core_modules::SOURCES
                        .iter()
                        .any(|(name, _)| *name == dep.alias)
                {
                    self.errors.push(Diagnostic::new(
                        "E3003",
                        format!(
                            "dependency alias `{}` conflicts with a standard/reserved module",
                            dep.alias
                        ),
                        Some(dep.span),
                    ));
                }
            }
        }
        Some(id)
    }
}

const MAX_DISCOVERY_ENTRIES_PER_DIRECTORY: usize = 65_536;
// Bound both traversal work and path vectors retained along recursive frames.
const MAX_PROJECT_DISCOVERY_ENTRIES: usize = 262_144;
const MAX_PROJECT_DISCOVERY_PATH_BYTES: usize = 64 * 1024 * 1024;

#[derive(Default)]
struct DiscoveryBudget {
    entries: usize,
    path_bytes: usize,
    exceeded: bool,
}

impl DiscoveryBudget {
    fn charge(&mut self, path_bytes: usize) -> bool {
        if self.exceeded {
            return false;
        }
        let Some(entries) = self.entries.checked_add(1) else {
            self.exceeded = true;
            return false;
        };
        let Some(bytes) = self.path_bytes.checked_add(path_bytes) else {
            self.exceeded = true;
            return false;
        };
        if entries > MAX_PROJECT_DISCOVERY_ENTRIES || bytes > MAX_PROJECT_DISCOVERY_PATH_BYTES {
            self.exceeded = true;
            return false;
        }
        self.entries = entries;
        self.path_bytes = bytes;
        true
    }
}

struct DiscoveryState<'a> {
    root: &'a Path,
    pkg: usize,
    excluded: &'a BTreeSet<PathBuf>,
    sources: &'a mut BTreeMap<ModuleKey, Source>,
    errors: &'a mut Vec<Diagnostic>,
    budget: &'a mut DiscoveryBudget,
}

fn discover(directory: &Path, prefix: &str, state: &mut DiscoveryState<'_>) {
    if state.budget.exceeded {
        return;
    }
    state
        .sources
        .entry((state.pkg, prefix.into()))
        .or_insert(Source::Empty);
    if prefix.split("::").count() > 64 || state.sources.len() >= 4096 {
        state
            .errors
            .push(error("compiler module depth/count limit exceeded"));
        return;
    }
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(e) => {
            state.errors.push(error(format!(
                "cannot discover {}: {e}",
                directory.display()
            )));
            return;
        }
    };
    let mut paths = Vec::new();
    for entry in entries {
        if paths.len() >= MAX_DISCOVERY_ENTRIES_PER_DIRECTORY {
            state
                .errors
                .push(error("compiler directory entry limit exceeded"));
            return;
        }
        match entry {
            Ok(entry) => {
                let path = entry.path();
                if !state.budget.charge(path.as_os_str().len()) {
                    state
                        .errors
                        .push(error("compiler project discovery budget exceeded"));
                    return;
                }
                paths.push(path);
            }
            Err(e) => state
                .errors
                .push(error(format!("cannot read directory entry: {e}"))),
        }
    }
    paths.sort();
    for path in paths {
        if state.budget.exceeded {
            break;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(e) => {
                state
                    .errors
                    .push(error(format!("cannot inspect {}: {e}", path.display())));
                continue;
            }
        };
        if metadata.file_type().is_symlink()
            || name.starts_with('.')
            || matches!(name, "target" | "tests" | "examples")
            || state.excluded.contains(&path)
        {
            continue;
        }
        if metadata.is_dir() {
            if path != state.root && path.join("package.kdl").exists() {
                continue;
            }
            if !config::identifier(name) {
                continue;
            }
            let key = if prefix.is_empty() {
                name.into()
            } else {
                format!("{prefix}::{name}")
            };
            if state
                .sources
                .get(&(state.pkg, key.clone()))
                .is_some_and(|s| !matches!(s, Source::Empty))
            {
                state.errors.push(error(format!(
                    "ambiguous module: file and directory named `{key}`"
                )));
            }
            discover(&path, &key, state);
        } else if metadata.is_file() && path.extension().and_then(|s| s.to_str()) == Some("cog") {
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            if !config::identifier(stem) {
                continue;
            }
            let key = if stem == "lib" && prefix.is_empty() {
                String::new()
            } else if prefix.is_empty() {
                stem.into()
            } else {
                format!("{prefix}::{stem}")
            };
            if let Some(previous) = state
                .sources
                .insert((state.pkg, key.clone()), Source::File(path.clone()))
                && (!matches!(previous, Source::Empty)
                    || (stem != "lib" && path.with_extension("").is_dir()))
            {
                state.errors.push(error(format!(
                    "ambiguous module `{key}` at {}",
                    path.display()
                )));
            }
        }
    }
}

fn core_sources(sources: &mut BTreeMap<ModuleKey, Source>) {
    sources.insert((CORE, String::new()), Source::Empty);
    for (name, text) in crate::core_modules::SOURCES {
        sources.insert(
            (CORE, (*name).into()),
            Source::Embedded(format!("<core/{name}.cog>"), (*text).into()),
        );
    }
}

fn relative(root: &Path, path: &Path) -> String {
    let a: Vec<_> = root.components().collect();
    let b: Vec<_> = path.components().collect();
    let common = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let mut parts: Vec<String> = a[common..].iter().map(|_| "..".into()).collect();
    parts.extend(
        b[common..]
            .iter()
            .map(|c| c.as_os_str().to_string_lossy().into_owned()),
    );
    if parts.is_empty() {
        ".".into()
    } else {
        parts.join("/")
    }
}

pub fn load(
    path: &Path,
    selected_target: Option<&str>,
    locked: bool,
) -> Result<Loaded, Vec<Diagnostic>> {
    prepare(path, selected_target, locked).map(Prepared::resolve)
}

pub fn prepare(
    path: &Path,
    selected_target: Option<&str>,
    locked: bool,
) -> Result<Prepared, Vec<Diagnostic>> {
    let explicit_file = path.is_file();
    let absolute = path
        .canonicalize()
        .map_err(|e| vec![error(format!("cannot open {}: {e}", path.display()))])?;
    let mut root = if explicit_file {
        absolute.parent().unwrap().to_path_buf()
    } else {
        absolute.clone()
    };
    if let Some(parent) = root.ancestors().find(|p| p.join("package.kdl").is_file()) {
        root = parent.to_owned();
    }
    let mut graph = Graph {
        packages: Vec::new(),
        by_path: BTreeMap::new(),
        aliases: BTreeMap::new(),
        alias_spans: BTreeMap::new(),
        errors: Vec::new(),
    };
    graph.package(&root, false);
    let targets = match config::targets(&root.join("build.kdl")) {
        Ok(v) => v,
        Err(errors) => {
            graph.errors.extend(errors);
            Vec::new()
        }
    };
    if explicit_file && selected_target.is_some() {
        graph
            .errors
            .push(error("--target requires a project directory"));
    }
    let package_name = graph
        .packages
        .first()
        .map(|p| p.metadata.name.clone())
        .unwrap_or_else(|| "main".into());
    let (entry, artifact_name) = if explicit_file {
        (
            absolute.clone(),
            absolute
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("main")
                .into(),
        )
    } else if let Some(name) = selected_target {
        if let Some(target) = targets.iter().find(|t| t.name == name) {
            (root.join(&target.entry), name.into())
        } else {
            graph
                .errors
                .push(error(format!("unknown build target `{name}`")));
            (root.join("main.cog"), name.into())
        }
    } else if root.join("src/main.cog").is_file() {
        (root.join("src/main.cog"), package_name)
    } else if root.join("main.cog").is_file() {
        (root.join("main.cog"), package_name)
    } else if targets.len() == 1 {
        (root.join(&targets[0].entry), package_name)
    } else {
        graph.errors.push(error(
            "no unambiguous entry point; expected src/main.cog, main.cog or --target",
        ));
        (root.join("main.cog"), package_name)
    };
    let mut resolved_includes = BTreeSet::new();
    if let Ok(text) = crate::limits::read_source_text(&entry) {
        let parsed = frontend::parse(entry.display().to_string(), &text);
        if parsed.diagnostics.is_empty() {
            for include in parsed.program.items.iter().filter_map(|item| match item {
                crate::ast::Item::ComptimeInclude {
                    package,
                    path,
                    span,
                    ..
                } => Some((package, path, span)),
                _ => None,
            }) {
                let (alias, requested_path, span) = include;
                resolved_includes.insert((alias.clone(), requested_path.clone()));
                if alias == "std" && requested_path.is_none() {
                    graph.bind_alias(0, alias, CORE, span.clone());
                    continue;
                }
                let target = if let Some(path) = requested_path {
                    std::env::current_dir()
                        .unwrap_or_else(|_| root.clone())
                        .join(path)
                } else if let Some(target) = graph.aliases.get(&(0, alias.clone())).copied() {
                    graph.packages[target].root.clone()
                } else {
                    let source_dir = root.join("src").join(alias);
                    if source_dir.is_dir() {
                        source_dir
                    } else {
                        root.join(alias)
                    }
                };
                if let Some(id) = graph.package(&target, false) {
                    graph.bind_alias(0, alias, id, span.clone());
                }
            }
        }
    }
    graph.validate_cycles();
    let mut sources = BTreeMap::new();
    let excluded: BTreeSet<_> = graph
        .aliases
        .values()
        .filter(|id| **id != CORE)
        .filter_map(|id| graph.packages.get(*id).map(|package| package.root.clone()))
        .collect();
    let mut discovery_budget = DiscoveryBudget::default();
    for (id, package) in graph.packages.iter().enumerate() {
        let dir = if package.root.join("src").is_dir() {
            package.root.join("src")
        } else {
            package.root.clone()
        };
        let mut state = DiscoveryState {
            root: &package.root,
            pkg: id,
            excluded: &excluded,
            sources: &mut sources,
            errors: &mut graph.errors,
            budget: &mut discovery_budget,
        };
        discover(&dir, "", &mut state);
    }
    let entry = entry.canonicalize().map_err(|e| {
        if graph.errors.is_empty() {
            vec![error(format!(
                "cannot read entry point {}: {e}",
                entry.display()
            ))]
        } else {
            std::mem::take(&mut graph.errors)
        }
    })?;
    let entry_key = sources.iter().find_map(|(key, source)| match source {
        Source::File(file) if file == &entry && key.0 == 0 => Some(key.clone()),
        _ => None,
    });
    let entry_key = entry_key.or_else(|| {
        if explicit_file {
            let key = (0, "$entry".to_string());
            sources.insert(key.clone(), Source::File(entry.clone()));
            Some(key)
        } else {
            None
        }
    });
    let entry_key = match entry_key {
        Some(key) => key,
        None => {
            graph.errors.push(error(
                "entry point must be a discovered .cog file inside the package source tree",
            ));
            (0, String::new())
        }
    };
    for (pkg, alias) in graph.aliases.keys() {
        if graph.aliases.get(&(*pkg, alias.clone())) == Some(&CORE) {
            continue;
        }
        if sources.contains_key(&(*pkg, alias.clone())) {
            graph.errors.push(error(format!(
                "dependency alias `{alias}` conflicts with a local module"
            )));
        }
    }
    core_sources(&mut sources);
    let lock_path = root.join("cog.lock");
    let lock = Lockfile {
        packages: graph
            .packages
            .iter()
            .map(|p| lockfile::Package {
                name: p.metadata.name.clone(),
                version: p.metadata.version.clone(),
                path: relative(&root, &p.root),
            })
            .collect(),
        dependencies: graph
            .aliases
            .iter()
            .filter_map(|((a, n), b)| (*b != CORE).then_some((*a, n.clone(), *b)))
            .collect(),
    };
    let resolution = if graph.errors.is_empty() {
        lock.encode().map_err(|e| vec![error(e)])?
    } else {
        String::new()
    };
    let mut lock_update = None;
    if graph.errors.is_empty() && (graph.packages.len() > 1 || lock_path.exists()) {
        let expected = &resolution;
        match crate::limits::read_source_text(&lock_path) {
            Ok(text) => match lockfile::decode(&lock_path.display().to_string(), &text) {
                Ok(old) if old.encode().as_ref() == Ok(expected) => {}
                Ok(_) if locked => graph.errors.push(error(
                    "cog.lock is stale; rebuild without --locked to update resolution",
                )),
                Ok(_) => lock_update = Some((lock_path, expected.clone())),
                Err(errors) => graph.errors.extend(errors),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !locked => {
                lock_update = Some((lock_path, expected.clone()))
            }
            Err(e) => graph
                .errors
                .push(error(format!("cannot use {}: {e}", lock_path.display()))),
        }
    }
    if !graph.errors.is_empty() {
        return Err(graph.errors);
    }
    let mut manifests = vec![root.join("build.kdl")];
    for package in &graph.packages {
        manifests.push(package.root.join("package.kdl"));
        manifests.push(package.root.join("dependencies.kdl"));
    }
    manifests.sort();
    let default_base = if explicit_file {
        std::env::current_dir().map_err(|e| {
            vec![error(format!(
                "cannot capture compiler working directory: {e}"
            ))]
        })?
    } else {
        root.clone()
    };
    let mut build_inputs =
        crate::build_inputs::BuildInputs::for_base(&default_base).map_err(|e| vec![error(e)])?;
    for ((package, _), source) in &sources {
        if *package == 0 || *package == CORE {
            continue;
        }
        let name = match source {
            Source::File(path) | Source::Snapshot(path, _) => path.display().to_string(),
            Source::Embedded(name, _) => name.clone(),
            Source::Empty => continue,
        };
        build_inputs
            .add_source(name, &graph.packages[*package].root)
            .map_err(|e| vec![error(e)])?;
    }
    Ok(Prepared {
        build_inputs,
        lock_update,
        artifact_name,
        root,
        sources,
        aliases: graph.aliases,
        entry: entry_key,
        resolved_includes,
        manifests,
        resolution,
    })
}

pub fn compile_source(name: &str, text: &str) -> Result<crate::backend::CompiledProgram, String> {
    compile_source_with_warnings(name, text).map(|(program, _)| program)
}

pub fn compile_source_with_warnings(
    name: &str,
    text: &str,
) -> Result<(crate::backend::CompiledProgram, Vec<Diagnostic>), String> {
    check_embedded_source_size(text)?;
    let inputs = crate::build_inputs::BuildInputs::from_current_dir()?;
    compile_source_with_inputs_and_warnings(name, text, &inputs)
}

/// Compile an in-memory source with an explicitly captured build-input base.
pub fn compile_source_with_inputs(
    name: &str,
    text: &str,
    inputs: &crate::build_inputs::BuildInputs,
) -> Result<crate::backend::CompiledProgram, String> {
    compile_source_with_inputs_and_warnings(name, text, inputs).map(|(program, _)| program)
}

pub fn compile_source_with_inputs_and_warnings(
    name: &str,
    text: &str,
    inputs: &crate::build_inputs::BuildInputs,
) -> Result<(crate::backend::CompiledProgram, Vec<Diagnostic>), String> {
    check_embedded_source_size(text)?;
    let parsed = frontend::parse(name, text);
    let resolved_includes: BTreeSet<_> = parsed
        .program
        .items
        .iter()
        .filter_map(|item| match item {
            crate::ast::Item::ComptimeInclude {
                package,
                path: None,
                ..
            } if package == "std" => Some((package.clone(), None)),
            _ => None,
        })
        .collect();
    let aliases = if !resolved_includes.is_empty() {
        BTreeMap::from([((0, "std".into()), CORE)])
    } else {
        BTreeMap::new()
    };
    let mut sources = BTreeMap::from([(
        (0, String::new()),
        Source::Embedded(name.into(), text.into()),
    )]);
    core_sources(&mut sources);
    let resolved = Resolver::new(sources, aliases, (0, String::new()))
        .with_resolved_includes(resolved_includes)
        .resolve();
    compile_with_inputs_and_warnings(
        resolved.program,
        resolved.diagnostics,
        &resolved.names,
        inputs,
    )
}

fn check_embedded_source_size(text: &str) -> Result<(), String> {
    if text.len() > crate::limits::CompilerLimits::default().max_source_bytes {
        Err("compiler source byte limit exceeded".into())
    } else {
        Ok(())
    }
}

pub fn compile(
    program: Program,
    diagnostics: Vec<Diagnostic>,
    names: &BTreeMap<String, String>,
) -> Result<crate::backend::CompiledProgram, String> {
    let inputs = crate::build_inputs::BuildInputs::from_current_dir()?;
    compile_with_inputs(program, diagnostics, names, &inputs)
}

pub fn compile_with_inputs(
    program: Program,
    diagnostics: Vec<Diagnostic>,
    names: &BTreeMap<String, String>,
    inputs: &crate::build_inputs::BuildInputs,
) -> Result<crate::backend::CompiledProgram, String> {
    compile_with_inputs_and_warnings(program, diagnostics, names, inputs)
        .map(|(program, _)| program)
}

pub fn compile_with_inputs_and_warnings(
    program: Program,
    mut diagnostics: Vec<Diagnostic>,
    names: &BTreeMap<String, String>,
    inputs: &crate::build_inputs::BuildInputs,
) -> Result<(crate::backend::CompiledProgram, Vec<Diagnostic>), String> {
    inputs.begin();
    let mut program = program;
    if let Err(message) = crate::ast_budget::program(&program) {
        diagnostics.push(Diagnostic::new("E4003", message, None));
    }
    if diagnostics.is_empty() {
        diagnostics.extend(crate::comptime::evaluate_program_with_inputs(
            &mut program,
            inputs,
        ));
    }
    let display = |mut text: String| {
        let mut pairs: Vec<_> = names
            .iter()
            .filter(|(bound, _)| bound.starts_with('$'))
            .collect();
        pairs.sort_by_key(|(bound, _)| std::cmp::Reverse(bound.len()));
        for (bound, name) in pairs {
            text = text.replace(bound, name);
        }
        text
    };
    let render = |mut diagnostics: Vec<Diagnostic>| {
        for diagnostic in &mut diagnostics {
            diagnostic.message = display(std::mem::take(&mut diagnostic.message));
            for note in &mut diagnostic.notes {
                *note = display(std::mem::take(note));
            }
        }
        // Source names, source lines and highlight coordinates are original
        // input, never text to rewrite while displaying bound symbol names.
        frontend::render(&diagnostics)
    };
    if !diagnostics.is_empty() {
        diagnostics.extend(crate::analysis::check(
            &program,
            &std::collections::HashMap::new(),
        ));
        return Err(render(diagnostics));
    }
    let warnings = crate::lint::check(&program);
    let mut compiled = crate::codegen::compile(&program).map_err(|e| render(e.diagnostics))?;
    for name in compiled.functions.values_mut() {
        if let Some(original) = names.get(name) {
            *name = original.clone();
        }
    }
    Ok((compiled, warnings))
}

pub fn write_lock(update: Option<(PathBuf, String)>) -> Result<(), String> {
    if let Some((path, text)) = update {
        use std::io::Write;
        let temporary = path.with_extension(format!("lock.tmp.{}", std::process::id()));
        let mut file = std::fs::File::create_new(&temporary)
            .map_err(|e| format!("cannot create {}: {e}", temporary.display()))?;
        let result = file.write_all(text.as_bytes());
        drop(file);
        if let Err(error) = result.and_then(|()| std::fs::rename(&temporary, &path)) {
            let _ = std::fs::remove_file(&temporary);
            return Err(format!("cannot write {}: {error}", path.display()));
        }
        eprintln!("updated {}", path.display());
    }
    Ok(())
}

#[cfg(test)]
mod discovery_tests {
    use super::{DiscoveryBudget, MAX_PROJECT_DISCOVERY_ENTRIES, MAX_PROJECT_DISCOVERY_PATH_BYTES};

    #[test]
    fn project_discovery_has_aggregate_entry_and_path_byte_budgets() {
        let mut entries = DiscoveryBudget::default();
        for _ in 0..MAX_PROJECT_DISCOVERY_ENTRIES {
            assert!(entries.charge(0));
        }
        assert!(!entries.charge(0));
        assert!(entries.exceeded);
        assert!(!entries.charge(0));

        let mut paths = DiscoveryBudget::default();
        assert!(paths.charge(MAX_PROJECT_DISCOVERY_PATH_BYTES));
        assert!(!paths.charge(1));
        assert!(paths.exceeded);
    }
}
