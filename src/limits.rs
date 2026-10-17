//! Finite compiler input budgets, independent of optional runtime authority.
use std::io::{self, Read};
use std::path::Path;

#[derive(Debug, Clone, Copy)]
pub struct CompilerLimits {
    pub max_source_bytes: usize,
    pub max_tokens: usize,
    pub max_diagnostics: usize,
    pub max_syntax_depth: usize,
    pub max_expression_depth: usize,
}

impl Default for CompilerLimits {
    fn default() -> Self {
        Self {
            max_source_bytes: 16 * 1024 * 1024,
            max_tokens: 262_144,
            max_diagnostics: 100,
            max_syntax_depth: 64,
            max_expression_depth: 128,
        }
    }
}

impl CompilerLimits {
    pub(crate) fn normalized(self) -> Self {
        Self {
            max_diagnostics: self.max_diagnostics.max(1),
            // Recursive frontend implementations have a tested hard ceiling.
            max_syntax_depth: self.max_syntax_depth.min(64),
            max_expression_depth: self.max_expression_depth.min(128),
            ..self
        }
    }
}

/// Read a compiler input without allocating beyond the default source budget.
pub fn read_source(path: &Path) -> io::Result<Vec<u8>> {
    let limit = CompilerLimits::default().max_source_bytes;
    let bytes = read_at_most(std::fs::File::open(path)?, limit + 1)?;
    if bytes.len() > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "compiler source byte limit exceeded",
        ));
    }
    Ok(bytes)
}

/// Read at most the caller's byte budget, with fallible incremental growth.
/// Callers include one sentinel byte when they need to detect oversized input.
pub(crate) fn read_at_most(mut input: impl Read, limit: usize) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut buffer = [0; 8192];
    while bytes.len() < limit {
        let available = (limit - bytes.len()).min(buffer.len());
        let count = match input.read(&mut buffer[..available]) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        bytes.try_reserve(count).map_err(|_| {
            io::Error::new(io::ErrorKind::OutOfMemory, "cannot allocate compiler input")
        })?;
        bytes.extend_from_slice(&buffer[..count]);
    }
    Ok(bytes)
}

pub(crate) fn read_source_text(path: &Path) -> io::Result<String> {
    String::from_utf8(read_source(path)?)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// Reject code points that KDL forbids in literals and bound recursive grammar
/// before entering the dependency parser. Comments and string contents are
/// scanned so comment markers inside strings cannot hide invalid code points.
pub(crate) fn check_kdl_depth(source: &str) -> Result<(), String> {
    check_kdl_code_points(source)?;

    let bytes = source.as_bytes();
    let mut cursor = 0usize;
    let mut braces = 0usize;
    let mut slashdash = 0usize;
    while cursor < bytes.len() {
        if bytes[cursor..].starts_with(b"//") {
            cursor += 2;
            while cursor < bytes.len() {
                let ch = source[cursor..].chars().next().expect("UTF-8 boundary");
                if matches!(
                    ch,
                    '\n' | '\r' | '\u{000c}' | '\u{0085}' | '\u{2028}' | '\u{2029}'
                ) {
                    break;
                }
                cursor += ch.len_utf8();
            }
            continue;
        }
        if bytes[cursor..].starts_with(b"/*") {
            cursor += 2;
            let mut comments = 1usize;
            while cursor < bytes.len() && comments > 0 {
                if bytes[cursor..].starts_with(b"/*") {
                    comments += 1;
                    if comments > 64 {
                        return Err("compiler KDL comment depth limit exceeded".into());
                    }
                    cursor += 2;
                } else if bytes[cursor..].starts_with(b"*/") {
                    comments -= 1;
                    cursor += 2;
                } else {
                    cursor += source[cursor..]
                        .chars()
                        .next()
                        .expect("UTF-8 boundary")
                        .len_utf8();
                }
            }
            continue;
        }
        if bytes[cursor..].starts_with(b"/-") {
            slashdash += 1;
            if slashdash > 64 {
                return Err("compiler KDL slashdash depth limit exceeded".into());
            }
            cursor += 2;
            continue;
        }
        let start = cursor;
        let mut hashes = 0usize;
        while bytes.get(cursor) == Some(&b'#') {
            hashes += 1;
            cursor += 1;
        }
        if bytes.get(cursor) == Some(&b'"') {
            let quotes = if bytes[cursor..].starts_with(b"\"\"\"") {
                3
            } else {
                1
            };
            cursor += quotes;
            while cursor < bytes.len() {
                if hashes == 0 && bytes[cursor] == b'\\' {
                    cursor += 1;
                    if cursor < bytes.len() {
                        cursor += source[cursor..]
                            .chars()
                            .next()
                            .expect("UTF-8 boundary")
                            .len_utf8();
                    }
                    continue;
                }
                if bytes[cursor..].starts_with(&b"\"\"\""[..quotes]) {
                    let end = cursor + quotes;
                    if bytes
                        .get(end..end.saturating_add(hashes))
                        .is_some_and(|suffix| suffix.iter().all(|byte| *byte == b'#'))
                    {
                        cursor = end + hashes;
                        break;
                    }
                }
                cursor += source[cursor..]
                    .chars()
                    .next()
                    .expect("UTF-8 boundary")
                    .len_utf8();
            }
            slashdash = 0;
            continue;
        }
        if hashes > 0 {
            slashdash = 0;
            continue;
        }
        cursor = start;
        let ch = source[cursor..].chars().next().expect("UTF-8 boundary");
        match ch {
            '{' => {
                braces += 1;
                if braces > 64 {
                    return Err("compiler KDL node depth limit exceeded".into());
                }
            }
            '}' => braces = braces.saturating_sub(1),
            _ => {}
        }
        if !ch.is_whitespace() && ch != '\\' && ch != '\u{feff}' {
            slashdash = 0;
        }
        cursor += ch.len_utf8();
    }
    Ok(())
}

fn check_kdl_code_points(source: &str) -> Result<(), String> {
    fn disallowed(ch: char) -> bool {
        matches!(
            ch,
            '\u{0000}'..='\u{0008}'
                | '\u{000E}'..='\u{001F}'
                | '\u{007F}'
                | '\u{200E}'..='\u{200F}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2066}'..='\u{2069}'
                | '\u{FEFF}'
        )
    }

    fn newline(ch: char) -> bool {
        matches!(
            ch,
            '\n' | '\r' | '\u{000B}' | '\u{000C}' | '\u{0085}' | '\u{2028}' | '\u{2029}'
        )
    }

    let bytes = source.as_bytes();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        // Visit strings before comments: `//` and `/*` are ordinary string
        // contents, and forbidden literal code points must be caught there.
        let string_start = cursor;
        let mut hashes = 0usize;
        while bytes.get(cursor) == Some(&b'#') {
            hashes += 1;
            cursor += 1;
        }
        if bytes.get(cursor) == Some(&b'"') {
            let quotes = if bytes[cursor..].starts_with(b"\"\"\"") {
                3
            } else {
                1
            };
            cursor += quotes;
            while cursor < bytes.len() {
                let ch = source[cursor..].chars().next().expect("UTF-8 boundary");
                if disallowed(ch) {
                    return Err("compiler KDL contains a disallowed code point".into());
                }
                if hashes == 0 && ch == '\\' {
                    cursor += ch.len_utf8();
                    if cursor < bytes.len() {
                        let escaped = source[cursor..].chars().next().expect("UTF-8 boundary");
                        if disallowed(escaped) {
                            return Err("compiler KDL contains a disallowed code point".into());
                        }
                        cursor += escaped.len_utf8();
                    }
                    continue;
                }
                if bytes[cursor..].starts_with(&b"\"\"\""[..quotes]) {
                    let end = cursor + quotes;
                    if bytes
                        .get(end..end.saturating_add(hashes))
                        .is_some_and(|suffix| suffix.iter().all(|byte| *byte == b'#'))
                    {
                        cursor = end + hashes;
                        break;
                    }
                }
                cursor += ch.len_utf8();
            }
            continue;
        }
        if hashes > 0 {
            // Consume a non-string run of `#` in one pass. Resetting to the
            // prefix would make long malformed runs quadratic.
            continue;
        }
        cursor = string_start;

        if bytes[cursor..].starts_with(b"//") {
            cursor += 2;
            while cursor < bytes.len() {
                let ch = source[cursor..].chars().next().expect("UTF-8 boundary");
                if newline(ch) {
                    break;
                }
                cursor += ch.len_utf8();
            }
            continue;
        }
        if bytes[cursor..].starts_with(b"/*") {
            cursor += 2;
            let mut comments = 1usize;
            while cursor < bytes.len() && comments > 0 {
                if bytes[cursor..].starts_with(b"/*") {
                    comments += 1;
                    if comments > 64 {
                        return Err("compiler KDL comment depth limit exceeded".into());
                    }
                    cursor += 2;
                } else if bytes[cursor..].starts_with(b"*/") {
                    comments -= 1;
                    cursor += 2;
                } else {
                    cursor += source[cursor..]
                        .chars()
                        .next()
                        .expect("UTF-8 boundary")
                        .len_utf8();
                }
            }
            continue;
        }

        let ch = source[cursor..].chars().next().expect("UTF-8 boundary");
        if disallowed(ch) && !(cursor == 0 && ch == '\u{FEFF}') {
            return Err("compiler KDL contains a disallowed code point".into());
        }
        cursor += ch.len_utf8();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_reads_stop_without_draining_and_retry_interrupted_reads() {
        struct Input {
            consumed: usize,
            interrupted: bool,
        }
        impl Read for Input {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                if !self.interrupted {
                    self.interrupted = true;
                    return Err(io::Error::from(io::ErrorKind::Interrupted));
                }
                self.consumed += buffer.len();
                buffer.fill(b'x');
                Ok(buffer.len())
            }
        }
        let mut input = Input {
            consumed: 0,
            interrupted: false,
        };
        assert_eq!(read_at_most(&mut input, 17).unwrap(), vec![b'x'; 17]);
        assert_eq!(input.consumed, 17);
        assert!(read_at_most(&mut input, 0).unwrap().is_empty());
        assert_eq!(input.consumed, 17);
    }

    #[test]
    fn kdl_preflight_rejects_disallowed_code_points_before_parsing() {
        let malformed = "\0\u{0002}n \n/-fal\u{001D}tfall";
        assert!(check_kdl_depth(malformed).is_err());
        assert!(check_kdl_depth("// ignored \0\u{001D}\nnode\n").is_ok());
        assert!(check_kdl_depth("\u{FEFF}node\n").is_ok());
        assert!(check_kdl_depth("node \u{FEFF}\n").is_err());
        assert!(check_kdl_depth("node \"bad\u{0000}value\"\n").is_err());
    }
}
