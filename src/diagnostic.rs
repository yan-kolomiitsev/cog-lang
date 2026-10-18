use std::ops::Range;
use std::rc::Rc;

const LINE_INDEX_BLOCK_BYTES: usize = 4096;

/// Immutable source ownership survives AST cloning, linking and lowering.
#[derive(Debug, PartialEq)]
pub struct SourceFile {
    pub name: String,
    pub text: String,
    line_index: Vec<LineCheckpoint>,
}

#[derive(Debug, PartialEq)]
struct LineCheckpoint {
    byte_offset: usize,
    line_count: usize,
    line_start: usize,
    column_count: usize,
}

impl SourceFile {
    pub fn new(name: impl Into<String>, text: impl Into<String>) -> Rc<Self> {
        let text = text.into();
        let mut line_index = Vec::with_capacity(text.len() / LINE_INDEX_BLOCK_BYTES + 1);
        let mut line_count = 0;
        let mut line_start = 0;
        let mut column_count = 0;
        for (block, bytes) in text.as_bytes().chunks(LINE_INDEX_BLOCK_BYTES).enumerate() {
            let byte_offset = block * LINE_INDEX_BLOCK_BYTES;
            line_index.push(LineCheckpoint {
                byte_offset,
                line_count,
                line_start,
                column_count,
            });
            for (index, byte) in bytes.iter().enumerate() {
                if *byte == b'\n' {
                    line_count += 1;
                    line_start = byte_offset + index + 1;
                    column_count = 0;
                } else if *byte & 0b1100_0000 != 0b1000_0000 {
                    column_count += 1;
                }
            }
        }
        if line_index.is_empty() {
            line_index.push(LineCheckpoint {
                byte_offset: 0,
                line_count: 0,
                line_start: 0,
                column_count: 0,
            });
        }
        Rc::new(Self {
            name: name.into(),
            text,
            line_index,
        })
    }

    pub fn span(self: &Rc<Self>, range: Range<usize>) -> SourceSpan {
        SourceSpan {
            source: self.clone(),
            range,
        }
    }

    pub fn line_column(&self, offset: usize) -> (usize, usize) {
        let offset = self.byte_boundary(offset);
        let (line, column, _) = self.line_info(offset);
        (line, column)
    }

    fn line_info(&self, offset: usize) -> (usize, usize, usize) {
        let checkpoint_index = (offset / LINE_INDEX_BLOCK_BYTES).min(self.line_index.len() - 1);
        let checkpoint = &self.line_index[checkpoint_index];
        let block = &self.text.as_bytes()[checkpoint.byte_offset..offset];
        let last_newline = block.iter().rposition(|byte| *byte == b'\n');
        let (line, line_start, column_count) = if let Some(index) = last_newline {
            let line_start = checkpoint.byte_offset + index + 1;
            let column_count = block[index + 1..]
                .iter()
                .filter(|byte| **byte & 0b1100_0000 != 0b1000_0000)
                .count();
            let line =
                checkpoint.line_count + block.iter().filter(|byte| **byte == b'\n').count() + 1;
            (line, line_start, column_count)
        } else {
            let column_count = checkpoint.column_count
                + block
                    .iter()
                    .filter(|byte| **byte & 0b1100_0000 != 0b1000_0000)
                    .count();
            (
                checkpoint.line_count + 1,
                checkpoint.line_start,
                column_count,
            )
        };
        let column = column_count + 1;
        (line, column, line_start)
    }

    fn line_end(&self, offset: usize) -> usize {
        self.text.as_bytes()[offset..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(self.text.len(), |distance| offset + distance)
    }

    fn byte_boundary(&self, offset: usize) -> usize {
        let mut offset = offset.min(self.text.len());
        while !self.text.is_char_boundary(offset) {
            offset -= 1;
        }
        offset
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SourceSpan {
    pub source: Rc<SourceFile>,
    pub range: Range<usize>,
}

/// Source coordinates that remain useful after the source text is discarded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceLocation {
    pub file: String,
    pub line: usize,
    pub column: usize,
}

impl SourceLocation {
    pub fn from_span(span: &SourceSpan) -> Self {
        let (line, column) = span.source.line_column(span.range.start);
        Self {
            file: span.source.name.clone(),
            line,
            column,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

impl Severity {
    fn label(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Diagnostic {
    pub code: &'static str,
    pub message: String,
    pub span: Option<SourceSpan>,
    pub notes: Vec<String>,
}

impl Diagnostic {
    pub fn new(code: &'static str, message: impl Into<String>, span: Option<SourceSpan>) -> Self {
        Self {
            code,
            message: message.into(),
            span,
            notes: Vec::new(),
        }
    }

    pub fn warning(
        code: &'static str,
        message: impl Into<String>,
        span: Option<SourceSpan>,
    ) -> Self {
        Self::new(code, message, span)
    }

    pub fn severity(&self) -> Severity {
        if self.code.starts_with('W') {
            Severity::Warning
        } else {
            Severity::Error
        }
    }

    pub fn note(mut self, note: impl Into<String>) -> Self {
        self.notes.push(note.into());
        self
    }
}

impl std::fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "{}[{}]: {}",
            self.severity().label(),
            self.code,
            self.message
        )?;
        if let Some(span) = &self.span {
            let start = span.source.byte_boundary(span.range.start);
            let (line, column, line_start) = span.source.line_info(start);
            writeln!(f, " --> {}:{line}:{column}", span.source.name)?;
            let line_end = span.source.line_end(start);
            // Bound repeated source context and underline allocation even for
            // a source-sized single line. Coordinates still name the full source.
            let excerpt_start = span.source.text[line_start..start]
                .char_indices()
                .rev()
                .nth(39)
                .map_or(line_start, |(offset, _)| line_start + offset);
            let excerpt_end = span.source.text[excerpt_start..line_end]
                .char_indices()
                .nth(160)
                .map_or(line_end, |(offset, _)| excerpt_start + offset);
            let prefix = if excerpt_start > line_start {
                "..."
            } else {
                ""
            };
            let suffix = if excerpt_end < line_end { "..." } else { "" };
            let text = &span.source.text[excerpt_start..excerpt_end];
            let highlight_end = span
                .source
                .byte_boundary(span.range.end)
                .min(excerpt_end)
                .max(start);
            let width = span.source.text[start..highlight_end]
                .chars()
                .count()
                .max(1);
            let padding = prefix.len() + span.source.text[excerpt_start..start].chars().count();
            writeln!(f, " {line} | {prefix}{text}{suffix}")?;
            writeln!(f, "   | {}{}", " ".repeat(padding), "^".repeat(width))?;
        }
        for note in &self.notes {
            writeln!(f, " note: {note}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_line_index_is_sparse_and_keeps_large_line_numbers_exact() {
        const SOURCE_BYTES: usize = 16 * 1024 * 1024;
        let text = format!("{}@", "\n".repeat(SOURCE_BYTES - 1));
        let source = SourceFile::new("many-lines.cog", text);
        assert!(source.line_index.len() <= SOURCE_BYTES / LINE_INDEX_BLOCK_BYTES + 1);
        assert_eq!(source.line_column(SOURCE_BYTES - 1), (SOURCE_BYTES, 1));

        let rendered = Diagnostic::new(
            "E0001",
            "invalid token",
            Some(source.span(SOURCE_BYTES - 1..SOURCE_BYTES)),
        )
        .to_string();
        assert!(rendered.contains(&format!("many-lines.cog:{SOURCE_BYTES}:1")));
        assert!(rendered.len() < 256);
    }

    #[test]
    fn sparse_line_index_counts_unicode_columns_across_byte_checkpoints() {
        let source = SourceFile::new("unicode.cog", "€".repeat(5000));
        assert_eq!(source.line_column(8193), (1, 2732));
        assert_eq!(source.line_column(8192), (1, 2731));
    }
}
