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

    /// Register a source at an explicit id (the semantic module graph's
    /// `SourceId` space) so spans that
    /// reference that id resolve to this text. Missing slots are filled with
    /// empty placeholders; an already-occupied slot keeps its first text.
    pub fn add_source_at(
        &mut self,
        id: SourceId,
        name: impl Into<String>,
        text: impl Into<String>,
    ) -> SourceId {
        let id_usize = id as usize;
        while self.files.len() <= id_usize {
            let placeholder = self.files.len() as SourceId;
            self.files
                .push(SourceFile::new(placeholder, String::new(), String::new()));
        }
        if self.files[id_usize].text.is_empty() && self.files[id_usize].name.is_empty() {
            self.files[id_usize] = SourceFile::new(id, name.into(), text.into());
        }
        id
    }

    /// Display name of the source registered at `id`.
    pub fn file_name(&self, id: SourceId) -> Option<&str> {
        self.file(id).map(|file| file.name.as_str())
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
    /// Exact byte-offset mapping from the lowered text back to the original
    /// source, generated *during* lowering by [`LoweringBuilder`]. Every
    /// parser provenance span referencing the lowered text is remapped through
    /// this table so semantic spans always slice the original source exactly.
    pub byte_mapping: ByteSpanMapping,
}

impl LoweredSource {
    pub fn identity(text: String) -> Self {
        let mapping = LineSpanMapping::identity(&text);
        let byte_mapping = ByteSpanMapping::identity(text.len());
        Self {
            text,
            mapping,
            byte_mapping,
        }
    }
}

/// One contiguous region of the lowered text and how it relates to the
/// original source.
///
/// Segments are recorded by [`LoweringBuilder`] while the lowered text is
/// produced, so they never involve searching the source afterwards. Copy
/// segments map lowered bytes 1:1 onto original bytes (equal byte lengths).
/// Inserted segments are lowered-only text (whitespace normalization,
/// inserted punctuation, synthetic tokens); they carry the original byte
/// offset at which the insertion occurred so spans landing inside them map
/// deterministically to that boundary. Removed original text occupies no
/// lowered bytes and is expressed implicitly by the original-offset gaps
/// between consecutive copy segments.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ByteSegment {
    /// `lowered_lo..lowered_hi` is a byte-for-byte copy of
    /// `original_lo..original_hi`. Both ranges have equal length.
    Copy {
        lowered_lo: usize,
        lowered_hi: usize,
        original_lo: usize,
        original_hi: usize,
    },
    /// `lowered_lo..lowered_hi` was inserted during lowering. `original_at`
    /// is the original byte offset of the insertion point.
    Inserted {
        lowered_lo: usize,
        lowered_hi: usize,
        original_at: usize,
    },
}

/// Exact byte-offset mapping from lowered text back to original source.
///
/// The segment list covers the lowered byte range `[0, lowered_len)`
/// contiguously in order: consecutive copy segments abut (each copy's
/// `lowered_lo` equals the previous segment's `lowered_hi`), and inserted
/// segments sit between copies. Original offsets strictly increase across
/// copy segments.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ByteSpanMapping {
    segments: Vec<ByteSegment>,
}

impl ByteSpanMapping {
    /// Identity mapping for a lowered text that equals the original.
    pub fn identity(text_len: usize) -> Self {
        let mut mapping = Self::default();
        if text_len > 0 {
            mapping.push_copy(0, text_len, 0, text_len);
        }
        mapping
    }

    /// Record a byte-for-byte copy of `original_lo..original_hi` appended at
    /// lowered offset `lowered_lo..lowered_hi`.
    pub fn push_copy(
        &mut self,
        lowered_lo: usize,
        lowered_hi: usize,
        original_lo: usize,
        original_hi: usize,
    ) {
        debug_assert_eq!(
            lowered_hi - lowered_lo,
            original_hi - original_lo,
            "copy segments must preserve byte length"
        );
        if let Some(ByteSegment::Copy {
            lowered_hi: prev_hi,
            original_hi: prev_orig_hi,
            ..
        }) = self.segments.last_mut()
        {
            // Merge adjacent copies that are contiguous on both sides.
            if *prev_hi == lowered_lo && *prev_orig_hi == original_lo {
                *prev_hi = lowered_hi;
                *prev_orig_hi = original_hi;
                return;
            }
        }
        debug_assert!(
            self.segments
                .last()
                .map(|last| lowered_lo >= last.lowered_hi())
                .unwrap_or(true),
            "copy segments must be appended in lowered order"
        );
        self.segments.push(ByteSegment::Copy {
            lowered_lo,
            lowered_hi,
            original_lo,
            original_hi,
        });
    }

    /// Record lowered-only text appended at `lowered_lo..lowered_hi`,
    /// inserted at original byte offset `original_at`.
    pub fn push_inserted(&mut self, lowered_lo: usize, lowered_hi: usize, original_at: usize) {
        debug_assert!(
            self.segments
                .last()
                .map(|last| lowered_lo >= last.lowered_hi())
                .unwrap_or(true),
            "inserted segments must be appended in lowered order"
        );
        self.segments.push(ByteSegment::Inserted {
            lowered_lo,
            lowered_hi,
            original_at,
        });
    }

    /// Map a lowered byte offset to the corresponding original byte offset.
    ///
    /// Offsets inside an inserted region map to the insertion boundary;
    /// offsets past the final segment map to the end of the last copy (or the
    /// insertion boundary for a trailing insertion).
    pub fn map_offset(&self, lowered_offset: usize) -> Option<usize> {
        let mut lo = 0usize;
        let mut hi = self.segments.len();
        while lo < hi {
            let mid = (lo + hi) / 2;
            let seg = &self.segments[mid];
            if lowered_offset < seg.lowered_lo() {
                hi = mid;
            } else if lowered_offset >= seg.lowered_hi() {
                lo = mid + 1;
            } else {
                return Some(match *seg {
                    ByteSegment::Copy {
                        lowered_lo,
                        original_lo,
                        ..
                    } => original_lo + (lowered_offset - lowered_lo),
                    ByteSegment::Inserted { original_at, .. } => original_at,
                });
            }
        }
        // Past the end: anchor at the end of the last copy, or the insertion
        // boundary for a trailing insertion.
        self.segments.last().map(|seg| match *seg {
            ByteSegment::Copy {
                lowered_hi,
                original_hi,
                ..
            } => original_hi + lowered_offset.saturating_sub(lowered_hi),
            ByteSegment::Inserted { original_at, .. } => original_at,
        })
    }

    /// Map a lowered span onto the original source. Returns `None` when the
    /// lowered span does not reference the lowered source id or an offset is
    /// out of range; offsets inside inserted text map to the insertion
    /// boundary, so the result is always a valid original byte range.
    pub fn map_span(
        &self,
        original_source_id: SourceId,
        lowered_span: Span,
        lowered_source_id: SourceId,
    ) -> Option<Span> {
        if lowered_span.source_id != lowered_source_id {
            return None;
        }
        let lo = self.map_offset(lowered_span.lo)?;
        let hi = self.map_offset(lowered_span.hi)?;
        Some(Span::new(original_source_id, lo, hi))
    }

    pub fn segments(&self) -> &[ByteSegment] {
        &self.segments
    }
}

impl ByteSegment {
    fn lowered_lo(&self) -> usize {
        match *self {
            ByteSegment::Copy { lowered_lo, .. } | ByteSegment::Inserted { lowered_lo, .. } => {
                lowered_lo
            }
        }
    }

    fn lowered_hi(&self) -> usize {
        match *self {
            ByteSegment::Copy { lowered_hi, .. } | ByteSegment::Inserted { lowered_hi, .. } => {
                lowered_hi
            }
        }
    }
}

/// Builds a [`LoweredSource`] while recording the exact byte mapping back to
/// the original source.
///
/// The original text is supplied once; copies are appended in original order
/// and inserted text is interleaved at the current original offset. The
/// finished [`LoweredSource`] carries both the lowered text and the
/// [`ByteSpanMapping`] produced during construction — callers never search
/// the source text afterwards.
#[derive(Clone, Debug)]
pub struct LoweringBuilder {
    original: String,
    lowered: String,
    original_cursor: usize,
    mapping: ByteSpanMapping,
}

impl LoweringBuilder {
    pub fn new(original: impl Into<String>) -> Self {
        Self {
            original: original.into(),
            lowered: String::new(),
            original_cursor: 0,
            mapping: ByteSpanMapping::default(),
        }
    }

    /// Append `original[range]` verbatim to the lowered text.
    pub fn copy_range(&mut self, range: Range<usize>) {
        debug_assert!(
            range.start >= self.original_cursor,
            "copy ranges must be appended in original order"
        );
        let lowered_lo = self.lowered.len();
        self.lowered.push_str(&self.original[range.clone()]);
        let lowered_hi = self.lowered.len();
        self.mapping
            .push_copy(lowered_lo, lowered_hi, range.start, range.end);
        self.original_cursor = range.end;
    }

    /// Append the remaining original text verbatim.
    pub fn copy_rest(&mut self) {
        if self.original_cursor < self.original.len() {
            self.copy_range(self.original_cursor..self.original.len());
        }
    }

    /// Append lowered-only text (whitespace normalization, inserted
    /// punctuation, synthetic tokens) at the current original offset.
    pub fn insert(&mut self, text: &str) {
        let lowered_lo = self.lowered.len();
        self.lowered.push_str(text);
        let lowered_hi = self.lowered.len();
        self.mapping
            .push_inserted(lowered_lo, lowered_hi, self.original_cursor);
    }

    /// Consume the builder, returning the lowered source with both the exact
    /// byte mapping and a consistent line mapping.
    pub fn finish(mut self) -> LoweredSource {
        self.copy_rest();
        let lowered_text = self.lowered;
        let byte_mapping = self.mapping;
        let line_mapping =
            LineSpanMapping::from_byte_mapping(&lowered_text, &self.original, &byte_mapping);
        LoweredSource {
            text: lowered_text,
            mapping: line_mapping,
            byte_mapping,
        }
    }

    pub fn original(&self) -> &str {
        &self.original
    }
}

impl LineSpanMapping {
    /// Derive the per-line mapping from an exact byte mapping: each lowered
    /// line maps to the original line containing its first byte.
    fn from_byte_mapping(
        lowered_text: &str,
        original_text: &str,
        byte_mapping: &ByteSpanMapping,
    ) -> Self {
        let lowered_starts = compute_line_starts(lowered_text);
        let original_starts = compute_line_starts(original_text);
        let mut lowered_to_original_line = Vec::with_capacity(lowered_starts.len());
        for &start in &lowered_starts {
            let original_offset = byte_mapping.map_offset(start).unwrap_or(start);
            let original_line = line_index_for_offset(&original_starts, original_offset)
                .map(|idx| idx + 1)
                .unwrap_or(1);
            lowered_to_original_line.push(original_line);
        }
        if lowered_to_original_line.is_empty() {
            lowered_to_original_line.push(1);
        }
        Self {
            lowered_to_original_line,
        }
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

#[cfg(test)]
mod byte_mapping_tests {
    use super::{LoweredSource, LoweringBuilder, Span};

    #[test]
    fn identity_mapping_maps_every_offset_to_itself() {
        let text = "fn add(a, b) { a + b }\n";
        let lowered = LoweredSource::identity(text.to_string());
        assert_eq!(lowered.text, text);
        for (offset, _) in text.char_indices() {
            assert_eq!(lowered.byte_mapping.map_offset(offset), Some(offset));
        }
        assert_eq!(
            lowered.byte_mapping.map_offset(text.len()),
            Some(text.len())
        );
    }

    #[test]
    fn identity_mapping_of_empty_source_maps_eof() {
        let lowered = LoweredSource::identity(String::new());
        assert_eq!(lowered.text, "");
        assert_eq!(lowered.byte_mapping.map_offset(0), None);
        assert_eq!(lowered.byte_mapping.segments().len(), 0);
    }

    #[test]
    fn builder_with_inserted_prefix_maps_offsets_past_the_insert() {
        let original = "let x = 1;\n";
        let mut builder = LoweringBuilder::new(original);
        builder.insert("// head\n");
        builder.copy_rest();
        let lowered = builder.finish();
        assert_eq!(lowered.text, "// head\nlet x = 1;\n");

        // Offsets inside the inserted region map to the insertion boundary (0).
        for offset in 0.."// head\n".len() {
            assert_eq!(lowered.byte_mapping.map_offset(offset), Some(0));
        }
        // Offsets inside the copied region map 1:1 onto the original.
        let copied_lo = "// head\n".len();
        for (i, (offset, _)) in original.char_indices().enumerate() {
            assert_eq!(
                lowered.byte_mapping.map_offset(copied_lo + i),
                Some(offset),
                "copied offset maps to the original offset"
            );
        }
        assert_eq!(
            lowered.byte_mapping.map_offset(lowered.text.len()),
            Some(original.len()),
            "trailing offset maps to original EOF"
        );
    }

    #[test]
    fn builder_span_mapping_maps_spans_to_original_ids() {
        let original = "let msg = \"変換\";\nprint(msg);\n";
        let mut builder = LoweringBuilder::new(original);
        builder.insert("// head\n");
        builder.copy_rest();
        let lowered = builder.finish();

        // A span covering the original `print(msg)` region in lowered text
        // maps back to the exact original byte range with the original id.
        let lowered_slice = "print(msg)";
        let lowered_lo = lowered.text.find(lowered_slice).unwrap();
        let span = Span::new(7, lowered_lo, lowered_lo + lowered_slice.len());
        let mapped = lowered
            .byte_mapping
            .map_span(7, span, 7)
            .expect("span maps");
        assert_eq!(mapped.source_id, 7);
        assert_eq!(&original[mapped.lo..mapped.hi], lowered_slice);

        // A span that references a different source id is left unmapped.
        assert_eq!(
            lowered.byte_mapping.map_span(7, Span::new(99, 0, 1), 7),
            None,
            "foreign source ids are not remapped"
        );
    }

    #[test]
    fn builder_with_removed_original_text_maps_across_the_gap() {
        // Remove the first 4 bytes (`let `) from the original by copying only
        // the tail; the original gap is implicit between copies.
        let original = "let x = 1;\n";
        let mut builder = LoweringBuilder::new(original);
        builder.copy_range(4..original.len());
        let lowered = builder.finish();
        assert_eq!(lowered.text, "x = 1;\n");
        assert_eq!(lowered.byte_mapping.map_offset(0), Some(4));
        assert_eq!(
            lowered.byte_mapping.map_offset(lowered.text.len()),
            Some(original.len())
        );
    }

    #[test]
    fn builder_line_mapping_tracks_inserted_lines() {
        let original = "let x = 1;\nprint(x);\n";
        let mut builder = LoweringBuilder::new(original);
        builder.insert("// head\n");
        builder.copy_rest();
        let lowered = builder.finish();
        // Lowered line 1 (inserted comment) maps to original line 1; the
        // copied lines map to their original lines. The trailing empty line
        // (after the final newline) maps to original line 3.
        assert_eq!(lowered.mapping.lowered_to_original_line, vec![1, 1, 2, 3]);
    }
}
