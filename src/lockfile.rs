//! COGLOCK 1: a deliberately small record format, independent of Cog/KDL.
use crate::diagnostic::{Diagnostic, SourceFile};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Package {
    pub name: String,
    pub version: String,
    pub path: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lockfile {
    pub packages: Vec<Package>,
    pub dependencies: Vec<(usize, String, usize)>,
}

fn quote(value: &str) -> String {
    let mut out = String::from("\"");
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

impl Lockfile {
    pub fn encode(&self) -> Result<String, String> {
        if self.packages.is_empty() {
            return Err("lock graph has no root package".into());
        }
        for p in &self.packages {
            if [&p.name, &p.version, &p.path].iter().any(|v| {
                v.is_empty()
                    || v.chars()
                        .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
            }) {
                return Err("lock graph contains an invalid package string".into());
            }
        }
        if self.dependencies.iter().any(|(a, n, b)| {
            *a >= self.packages.len() || *b >= self.packages.len() || !crate::config::identifier(n)
        }) {
            return Err("lock graph contains an invalid dependency".into());
        }
        let mut order: Vec<_> = (1..self.packages.len()).collect();
        order.sort_by_key(|i| &self.packages[*i]);
        order.insert(0, 0);
        let ids: BTreeMap<_, _> = order
            .iter()
            .enumerate()
            .map(|(id, old)| (*old, id))
            .collect();
        let mut out = String::from("COGLOCK 1\nroot 0\n");
        for (id, old) in order.iter().enumerate() {
            let p = &self.packages[*old];
            out.push_str(&format!(
                "pkg {id} {} {} path {}\n",
                quote(&p.name),
                quote(&p.version),
                quote(&p.path)
            ));
        }
        let mut edges: Vec<_> = self
            .dependencies
            .iter()
            .map(|(a, n, b)| (ids[a], n, ids[b]))
            .collect();
        edges.sort();
        for (a, n, b) in edges {
            out.push_str(&format!("dep {a} {} {b}\n", quote(n)));
        }
        Ok(out)
    }
}

#[derive(Debug)]
struct Token {
    value: String,
    quoted: bool,
}

fn tokens(line: &str) -> Result<Vec<Token>, String> {
    let mut out = Vec::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if matches!(c, ' ' | '\t') {
            continue;
        }
        let quoted = c == '"';
        let mut value = String::new();
        if quoted {
            let mut closed = false;
            while let Some(c) = chars.next() {
                match c {
                    '"' => {
                        closed = true;
                        break;
                    }
                    '\\' => value.push(match chars.next() {
                        Some('"') => '"',
                        Some('\\') => '\\',
                        Some('n') => '\n',
                        Some('r') => '\r',
                        Some('t') => '\t',
                        _ => return Err("invalid string escape".into()),
                    }),
                    c if c.is_control() => return Err("unescaped control character".into()),
                    _ => value.push(c),
                }
            }
            if !closed {
                return Err("unterminated string".into());
            }
            if chars.peek().is_some_and(|c| !matches!(c, ' ' | '\t')) {
                return Err("expected whitespace after string".into());
            }
        } else {
            value.push(c);
            while chars.peek().is_some_and(|c| !matches!(c, ' ' | '\t')) {
                value.push(chars.next().expect("peeked character"));
            }
        }
        out.push(Token { value, quoted });
    }
    Ok(out)
}

fn id(token: &Token) -> Result<usize, String> {
    if token.quoted
        || token.value.is_empty()
        || !token.value.bytes().all(|c| c.is_ascii_digit())
        || (token.value.len() > 1 && token.value.starts_with('0'))
    {
        return Err("expected an unquoted u32 package ID".into());
    }
    token
        .value
        .parse::<u32>()
        .map(|n| n as usize)
        .map_err(|_| "package ID out of range".into())
}
fn string(token: &Token) -> Result<String, String> {
    if token.quoted {
        Ok(token.value.clone())
    } else {
        Err("expected a quoted string".into())
    }
}

pub fn decode(name: &str, text: &str) -> Result<Lockfile, Vec<Diagnostic>> {
    let source = SourceFile::new(name, text);
    let mut errors = Vec::new();
    let mut packages = BTreeMap::new();
    let mut edges = Vec::new();
    let mut root = None;
    let mut offset = 0;
    let mut identities = BTreeSet::new();
    let mut aliases = BTreeSet::new();
    for (line_no, raw) in text.split_inclusive('\n').enumerate() {
        let line = raw.trim_end_matches(['\r', '\n']);
        let span = source.span(offset..offset + line.len());
        offset += raw.len();
        let result = (|| -> Result<(), String> {
            let ts = tokens(line)?;
            if line_no == 0 {
                if ts.len() != 2
                    || ts[0].quoted
                    || ts[0].value != "COGLOCK"
                    || ts[1].quoted
                    || ts[1].value != "1"
                {
                    return Err("expected COGLOCK 1 header".into());
                }
                return Ok(());
            }
            if ts.is_empty() {
                return Ok(());
            }
            if ts[0].quoted {
                return Err("record kind cannot be quoted".into());
            }
            match ts[0].value.as_str() {
                "root" if ts.len() == 2 => {
                    if root.replace(id(&ts[1])?).is_some() {
                        return Err("duplicate root record".into());
                    }
                }
                "pkg" if ts.len() == 6 && !ts[4].quoted && ts[4].value == "path" => {
                    let key = id(&ts[1])?;
                    let p = Package {
                        name: string(&ts[2])?,
                        version: string(&ts[3])?,
                        path: string(&ts[5])?,
                    };
                    if p.name.is_empty() || p.version.is_empty() || p.path.is_empty() {
                        return Err("package name, version and path must be nonempty".into());
                    }
                    if std::path::Path::new(&p.path).is_absolute() {
                        return Err("package source must be a relative path".into());
                    }
                    if !identities.insert(p.clone()) {
                        return Err("duplicate package identity".into());
                    }
                    if packages.insert(key, p).is_some() {
                        return Err(format!("duplicate package ID {key}"));
                    }
                }
                "dep" if ts.len() == 4 => {
                    let a = id(&ts[1])?;
                    let alias = string(&ts[2])?;
                    let b = id(&ts[3])?;
                    if !crate::config::identifier(&alias) {
                        return Err("invalid dependency alias".into());
                    }
                    if !aliases.insert((a, alias.clone())) {
                        return Err("duplicate dependency alias in package".into());
                    }
                    edges.push((a, alias, b));
                }
                _ => return Err("unknown record or incorrect number/type of fields".into()),
            }
            Ok(())
        })();
        if let Err(message) = result {
            errors.push(Diagnostic::new("E3101", message, Some(span)));
        }
    }
    let mut structural =
        |message: String| errors.push(Diagnostic::new("E3101", message, Some(source.span(0..0))));
    if text.is_empty() {
        structural("missing COGLOCK 1 header".into());
    }
    if root != Some(0) || !packages.contains_key(&0) {
        structural("expected root 0 and its package record".into());
    }
    if packages.keys().copied().ne(0..packages.len()) {
        structural("package IDs must be contiguous from 0".into());
    }
    for (a, _, b) in &edges {
        if !packages.contains_key(a) || !packages.contains_key(b) {
            structural(format!(
                "dependency references nonexistent package {a} or {b}"
            ));
        }
    }
    let mut reachable = BTreeSet::from([0]);
    loop {
        let before = reachable.len();
        for (a, _, b) in &edges {
            if reachable.contains(a) {
                reachable.insert(*b);
            }
        }
        if before == reachable.len() {
            break;
        }
    }
    if packages.keys().any(|id| !reachable.contains(id)) {
        structural("unreachable package record".into());
    }
    if errors.is_empty() {
        Ok(Lockfile {
            packages: packages.into_values().collect(),
            dependencies: edges,
        })
    } else {
        Err(errors)
    }
}
