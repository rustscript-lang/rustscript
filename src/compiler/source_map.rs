use std::ops::Range;

pub type SourceId = u32;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Span {
    pub source_id: SourceId,
    pub lo: usize,
    pub hi: usize,
}

impl Span {
    pub fn new(source_id: SourceId, lo: usize, hi: usize) -> Self {
        if lo <= hi {
            Self { source_id, lo, hi }
        } else {
            Self {
                source_id,
                lo: hi,
                hi: lo,
            }
        }
    }

    pub fn len(self) -> usize {
        self.hi.saturating_sub(self.lo)
    }

    pub fn is_empty(self) -> bool {
        self.lo == self.hi
    }
}

#[derive(Clone, Debug)]
pub struct SourceFile {
    pub id: SourceId,
    pub name: String,
    pub text: String,
    line_starts: Vec<usize>,
}

impl SourceFile {
    fn new(id: SourceId, name: String, text: String) -> Self {
        let line_starts = compute_line_starts(&text);
        Self {
            id,
            name,
            text,
            line_starts,
        }
    }

    pub fn line_count(&self) -> usize {
        self.line_starts.len()
    }

    pub fn line_col_for_offset(&self, offset: usize) -> Option<(usize, usize)> {
        if offset > self.text.len() {
            return None;
        }
        let line_idx = line_index_for_offset(&self.line_starts, offset)?;
        let line_start = self.line_starts[line_idx];
        let col = self.text[line_start..offset].chars().count() + 1;
        Some((line_idx + 1, col))
    }

    pub fn line_span(&self, line: usize) -> Option<Range<usize>> {
        if line == 0 || line > self.line_starts.len() {
            return None;
        }
        let idx = line - 1;
        let start = self.line_starts[idx];
        let end = if idx + 1 < self.line_starts.len() {
            self.line_starts[idx + 1]
        } else {
            self.text.len()
        };
        let line_text = &self.text[start..end];
        let trimmed_end = line_text.trim_end_matches(['\n', '\r']).len();
        Some(start..start + trimmed_end)
    }

    pub fn line_text(&self, line: usize) -> Option<&str> {
        let range = self.line_span(line)?;
        self.text.get(range)
    }

    pub fn line_col_to_offset(&self, line: usize, col: usize) -> Option<usize> {
        if line == 0 || line > self.line_starts.len() || col == 0 {
            return None;
        }
        let line_range = self.line_span(line)?;
        let mut byte = line_range.start;
        let mut current_col = 1usize;
        while byte < line_range.end && current_col < col {
            let ch = self.text[byte..].chars().next()?;
            byte += ch.len_utf8();
            current_col += 1;
        }
        Some(byte)
    }
}

#[derive(Clone, Debug, Default)]
pub struct SourceMap {
    files: Vec<SourceFile>,
}

impl SourceMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_source(&mut self, name: impl Into<String>, text: impl Into<String>) -> SourceId {
        let id = self.files.len() as SourceId;
        self.files
            .push(SourceFile::new(id, name.into(), text.into()));
        id
    }

    pub fn file(&self, id: SourceId) -> Option<&SourceFile> {
        self.files.get(id as usize)
    }

    pub fn source_id_by_name(&self, name: &str) -> Option<SourceId> {
        self.files
            .iter()
            .find(|file| file.name == name)
            .map(|file| file.id)
    }

    pub fn source(&self, id: SourceId) -> Option<&str> {
        self.file(id).map(|file| file.text.as_str())
    }

    pub fn line_span(&self, id: SourceId, line: usize) -> Option<Span> {
        let file = self.file(id)?;
        let range = file.line_span(line)?;
        Some(Span::new(id, range.start, range.end))
    }

    pub fn line_col_for_offset(&self, id: SourceId, offset: usize) -> Option<(usize, usize)> {
        self.file(id)?.line_col_for_offset(offset)
    }

    pub fn line_col_to_offset(&self, id: SourceId, line: usize, col: usize) -> Option<usize> {
        self.file(id)?.line_col_to_offset(line, col)
    }

    pub fn span_text(&self, span: Span) -> Option<&str> {
        let file = self.file(span.source_id)?;
        file.text.get(span.lo..span.hi)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineSpanMapping {
    pub lowered_to_original_line: Vec<usize>,
}

impl LineSpanMapping {
    pub fn identity(source: &str) -> Self {
        let lines = source.lines().count().max(1);
        Self {
            lowered_to_original_line: (1..=lines).collect(),
        }
    }

    pub fn map_span(
        &self,
        source_map: &SourceMap,
        lowered_source_id: SourceId,
        original_source_id: SourceId,
        lowered_span: Span,
    ) -> Option<Span> {
        if lowered_span.source_id != lowered_source_id {
            return None;
        }
        let (lowered_line, lowered_col) =
            source_map.line_col_for_offset(lowered_source_id, lowered_span.lo)?;
        let original_line = *self
            .lowered_to_original_line
            .get(lowered_line.saturating_sub(1))
            .unwrap_or(&lowered_line);
        let original_file = source_map.file(original_source_id)?;
        let line_range = original_file.line_span(original_line)?;
        let lo = original_file
            .line_col_to_offset(original_line, lowered_col)
            .unwrap_or(line_range.start);
        let hi = if lowered_span.is_empty() {
            lo
        } else {
            line_range.end.min(lo.saturating_add(lowered_span.len()))
        };
        Some(Span::new(original_source_id, lo, hi))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoweredSource {
    pub text: String,
    pub mapping: LineSpanMapping,
}

impl LoweredSource {
    pub fn identity(text: String) -> Self {
        let mapping = LineSpanMapping::identity(&text);
        Self { text, mapping }
    }
}

fn compute_line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    for (idx, ch) in text.char_indices() {
        if ch == '\n' {
            starts.push(idx + 1);
        }
    }
    if starts.is_empty() {
        starts.push(0);
    }
    starts
}

fn line_index_for_offset(line_starts: &[usize], offset: usize) -> Option<usize> {
    if line_starts.is_empty() {
        return None;
    }
    let mut lo = 0usize;
    let mut hi = line_starts.len();
    while lo < hi {
        let mid = (lo + hi) / 2;
        if line_starts[mid] <= offset {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    Some(lo.saturating_sub(1))
}
