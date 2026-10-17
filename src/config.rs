//! KDL syntax and strict, purpose-specific configuration schemas.
use crate::diagnostic::{Diagnostic, SourceFile, SourceSpan};
use kdl::{KdlDocument, KdlNode};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

pub const MAX_CONFIGURATION_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug)]
pub struct Package {
    pub name: String,
    pub version: String,
}

#[derive(Clone, Debug)]
pub struct Dependency {
    pub alias: String,
    pub path: PathBuf,
    pub span: SourceSpan,
}

#[derive(Clone, Debug)]
pub struct Target {
    pub name: String,
    pub entry: PathBuf,
}

pub fn valid_package_name(name: &str) -> bool {
    !name.is_empty()
        && !matches!(name, "." | "..")
        && !name
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '\\'))
}

pub fn identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

fn configuration_size_error() -> Vec<Diagnostic> {
    vec![Diagnostic::new(
        "E3002",
        "compiler configuration byte limit (256 KiB) exceeded",
        None,
    )]
}

fn read_configuration(path: &Path) -> Result<String, Vec<Diagnostic>> {
    let read =
        || crate::limits::read_at_most(std::fs::File::open(path)?, MAX_CONFIGURATION_BYTES + 1);
    let read_error = |e| {
        vec![Diagnostic::new(
            "E3001",
            format!("cannot read {}: {e}", path.display()),
            None,
        )]
    };
    let bytes = read().map_err(read_error)?;
    if bytes.len() > MAX_CONFIGURATION_BYTES {
        return Err(configuration_size_error());
    }
    String::from_utf8(bytes)
        .map_err(|e| read_error(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))
}

fn document(path: &Path, text: &str) -> Result<(Rc<SourceFile>, KdlDocument), Vec<Diagnostic>> {
    if text.len() > MAX_CONFIGURATION_BYTES {
        return Err(configuration_size_error());
    }
    let source = SourceFile::new(path.display().to_string(), text);
    if let Err(message) = crate::limits::check_kdl_depth(&source.text) {
        return Err(vec![Diagnostic::new(
            "E3002",
            message,
            Some(source.span(0..0)),
        )]);
    }
    match KdlDocument::parse_v2(&source.text) {
        Ok(doc) => Ok((source, doc)),
        Err(error) => Err(error
            .diagnostics
            .iter()
            .map(|d| {
                let mut diag = Diagnostic::new(
                    "E3002",
                    d.to_string(),
                    Some(source.span(d.span.offset()..d.span.offset() + d.span.len())),
                );
                if let Some(help) = &d.help {
                    diag = diag.note(help);
                }
                diag
            })
            .collect()),
    }
}

fn error(
    errors: &mut Vec<Diagnostic>,
    source: &Rc<SourceFile>,
    node: &KdlNode,
    message: impl Into<String>,
) {
    let s = node.span();
    errors.push(Diagnostic::new(
        "E3003",
        message,
        Some(source.span(s.offset()..s.offset() + s.len())),
    ));
}

/// Collect entries without using KDL's last-property-wins lookup.
fn fields(
    node: &KdlNode,
    allowed: &[&str],
    source: &Rc<SourceFile>,
    errors: &mut Vec<Diagnostic>,
) -> (Vec<String>, BTreeMap<String, String>) {
    if node.ty().is_some() || node.children().is_some() {
        error(
            errors,
            source,
            node,
            "type annotations and child blocks are not allowed here",
        );
    }
    let mut args = Vec::new();
    let mut props = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for entry in node.entries() {
        let span = entry.span();
        let location = Some(source.span(span.offset()..span.offset() + span.len()));
        if entry.ty().is_some() {
            errors.push(Diagnostic::new(
                "E3003",
                "type annotations are not allowed here",
                location.clone(),
            ));
        }
        let value = entry.value().as_string();
        if let Some(name) = entry.name().map(|n| n.value()) {
            if !allowed.contains(&name) {
                errors.push(Diagnostic::new(
                    "E3003",
                    format!("unknown property `{name}`"),
                    location.clone(),
                ));
            }
            if !seen.insert(name.to_owned()) {
                errors.push(Diagnostic::new(
                    "E3003",
                    format!("duplicate property `{name}`"),
                    location.clone(),
                ));
            }
            if let Some(value) = value {
                props.insert(name.into(), value.into());
            }
        } else if let Some(value) = value {
            args.push(value.into());
        }
        if value.is_some_and(|s| {
            s.chars()
                .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
        }) {
            errors.push(Diagnostic::new(
                "E3003",
                "unsupported control character in string",
                location.clone(),
            ));
        }
        if value.is_none() {
            errors.push(Diagnostic::new(
                "E3003",
                "expected a string value",
                location,
            ));
        }
    }
    (args, props)
}

pub fn package(path: &Path) -> Result<Package, Vec<Diagnostic>> {
    package_from_source(path, &read_configuration(path)?)
}

/// Parse a package schema without filesystem access. The path labels diagnostics.
pub fn package_from_source(path: &Path, text: &str) -> Result<Package, Vec<Diagnostic>> {
    let (source, doc) = document(path, text)?;
    let mut errors = Vec::new();
    let mut result = None;
    for node in doc.nodes() {
        if node.name().value() != "package" {
            error(
                &mut errors,
                &source,
                node,
                format!("unknown node `{}`; expected package", node.name()),
            );
            continue;
        }
        let (args, props) = fields(node, &["version"], &source, &mut errors);
        if result.is_some() {
            error(&mut errors, &source, node, "duplicate package declaration");
        }
        if args.len() != 1
            || args.first().is_none_or(|v| v.is_empty())
            || props.get("version").is_none_or(|v| v.is_empty())
        {
            error(
                &mut errors,
                &source,
                node,
                "expected: package \"name\" version=\"version\"",
            );
        } else if !valid_package_name(&args[0]) {
            error(
                &mut errors,
                &source,
                node,
                "package name must be a filename component without separators or control characters",
            );
        } else {
            result = Some(Package {
                name: args[0].clone(),
                version: props["version"].clone(),
            });
        }
    }
    if result.is_none() && errors.is_empty() {
        errors.push(Diagnostic::new(
            "E3003",
            "missing package declaration",
            Some(source.span(0..0)),
        ));
    }
    if errors.is_empty() {
        Ok(result.expect("validated package"))
    } else {
        Err(errors)
    }
}

pub fn dependencies(path: &Path) -> Result<Vec<Dependency>, Vec<Diagnostic>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    dependencies_from_source(path, &read_configuration(path)?)
}

/// Parse dependency declarations without opening or resolving their paths.
pub fn dependencies_from_source(
    path: &Path,
    text: &str,
) -> Result<Vec<Dependency>, Vec<Diagnostic>> {
    let (source, doc) = document(path, text)?;
    let mut errors = Vec::new();
    let mut result = Vec::new();
    let mut seen = BTreeSet::new();
    for node in doc.nodes() {
        let alias = node.name().value();
        let (args, props) = fields(node, &["path"], &source, &mut errors);
        if !identifier(alias) || !seen.insert(alias.to_owned()) {
            error(
                &mut errors,
                &source,
                node,
                format!("invalid or duplicate dependency alias `{alias}`"),
            );
        }
        if !args.is_empty() || props.get("path").is_none_or(|p| p.is_empty()) {
            error(
                &mut errors,
                &source,
                node,
                "expected: alias path=\"directory\"",
            );
        } else {
            let s = node.span();
            result.push(Dependency {
                alias: alias.into(),
                path: props["path"].clone().into(),
                span: source.span(s.offset()..s.offset() + s.len()),
            });
        }
    }
    if errors.is_empty() {
        Ok(result)
    } else {
        Err(errors)
    }
}

pub fn targets(path: &Path) -> Result<Vec<Target>, Vec<Diagnostic>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    targets_from_source(path, &read_configuration(path)?)
}

/// Parse build targets without opening source files or creating artifacts.
pub fn targets_from_source(path: &Path, text: &str) -> Result<Vec<Target>, Vec<Diagnostic>> {
    let (source, doc) = document(path, text)?;
    let mut errors = Vec::new();
    let mut result = Vec::new();
    let mut seen = BTreeSet::new();
    for node in doc.nodes() {
        let (args, props) = fields(node, &["entry"], &source, &mut errors);
        if node.name().value() != "target"
            || args.len() != 1
            || args.first().is_none_or(|v| !identifier(v))
            || props.get("entry").is_none_or(|p| p.is_empty())
        {
            error(
                &mut errors,
                &source,
                node,
                "expected: target \"name\" entry=\"relative/file.cog\"",
            );
        } else if !seen.insert(args[0].clone()) {
            error(
                &mut errors,
                &source,
                node,
                format!("duplicate target `{}`", args[0]),
            );
        } else {
            let entry = PathBuf::from(&props["entry"]);
            if entry.is_absolute()
                || entry
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                error(
                    &mut errors,
                    &source,
                    node,
                    "target entry must be inside the package",
                );
            } else {
                result.push(Target {
                    name: args[0].clone(),
                    entry,
                });
            }
        }
    }
    if errors.is_empty() {
        Ok(result)
    } else {
        Err(errors)
    }
}
