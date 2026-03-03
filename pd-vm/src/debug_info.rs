#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArgInfo {
    pub name: String,
    pub position: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DebugFunction {
    pub name: String,
    pub args: Vec<ArgInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalInfo {
    pub name: String,
    pub index: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineInfo {
    pub offset: u32,
    pub line: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DebugInfo {
    pub source: Option<String>,
    pub lines: Vec<LineInfo>,
    pub functions: Vec<DebugFunction>,
    pub locals: Vec<LocalInfo>,
}

impl DebugInfo {
    pub fn line_for_offset(&self, offset: usize) -> Option<u32> {
        let offset = offset as u32;
        if self.lines.is_empty() {
            return None;
        }
        let mut lo = 0;
        let mut hi = self.lines.len();
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.lines[mid].offset <= offset {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo == 0 {
            None
        } else {
            Some(self.lines[lo - 1].line)
        }
    }

    pub fn offsets_for_line(&self, line: u32) -> Vec<u32> {
        self.lines
            .iter()
            .filter(|info| info.line == line)
            .map(|info| info.offset)
            .collect()
    }

    pub fn source_line(&self, line: u32) -> Option<String> {
        let source = self.source.as_ref()?;
        let index = line.checked_sub(1)? as usize;
        source.lines().nth(index).map(|text| text.to_string())
    }

    pub fn local_index(&self, name: &str) -> Option<u8> {
        self.locals
            .iter()
            .find(|local| local.name == name)
            .map(|local| local.index)
    }
}

#[derive(Default)]
pub struct DebugInfoBuilder {
    source: Option<String>,
    lines: Vec<LineInfo>,
    functions: Vec<DebugFunction>,
    locals: Vec<LocalInfo>,
    last_offset: Option<u32>,
}

impl DebugInfoBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_source(&mut self, source: String) {
        self.source = Some(source);
    }

    pub fn add_function(&mut self, name: String, args: Vec<String>) {
        let args = args
            .into_iter()
            .enumerate()
            .map(|(idx, name)| ArgInfo {
                name,
                position: idx as u8,
            })
            .collect();
        self.functions.push(DebugFunction { name, args });
    }

    pub fn add_local(&mut self, name: String, index: u8) {
        if self
            .locals
            .iter()
            .any(|local| local.name == name || local.index == index)
        {
            return;
        }
        self.locals.push(LocalInfo { name, index });
    }

    pub fn mark_line(&mut self, offset: u32, line: u32) {
        if self.last_offset == Some(offset) {
            if let Some(last) = self.lines.last_mut()
                && last.offset == offset
            {
                // Keep the most recent line mapping for this offset so non-emitting
                // statements (e.g., import declarations) do not pin stale lines.
                last.line = line;
            }
            return;
        }
        self.lines.push(LineInfo { offset, line });
        self.last_offset = Some(offset);
    }

    pub fn finish(self) -> Option<DebugInfo> {
        if self.source.is_none()
            && self.lines.is_empty()
            && self.functions.is_empty()
            && self.locals.is_empty()
        {
            return None;
        }
        Some(DebugInfo {
            source: self.source,
            lines: self.lines,
            functions: self.functions,
            locals: self.locals,
        })
    }
}
