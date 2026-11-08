//! Blocks and rich text.
//!
//! The block vocabulary is the Notion block set, cut to what a text note needs.
//! Rich text is a Quill-style delta: a list of spans, each carrying a set of
//! inline marks. Both are borrowed shapes — see `docs/ARCHITECTURE.md`.

use std::fmt;

use loro::TreeID;

/// Stable identity of a block inside a note.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockId(pub TreeID);

impl BlockId {
    pub fn peer(self) -> u64 {
        self.0.peer
    }

    pub fn counter(self) -> i32 {
        self.0.counter
    }
}

impl fmt::Display for BlockId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.0.peer, self.0.counter)
    }
}

impl fmt::Debug for BlockId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl std::str::FromStr for BlockId {
    type Err = ParseBlockIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (peer, counter) = s.split_once('@').ok_or(ParseBlockIdError)?;
        Ok(BlockId(TreeID::new(
            peer.parse().map_err(|_| ParseBlockIdError)?,
            counter.parse().map_err(|_| ParseBlockIdError)?,
        )))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("block id must look like `peer@counter`")]
pub struct ParseBlockIdError;

/// The Notion block vocabulary, minus the database/media types a text note does
/// not need. Every kind round-trips through markdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    Paragraph,
    Heading1,
    Heading2,
    Heading3,
    BulletedListItem,
    NumberedListItem,
    /// Checkbox item; state lives in [`crate::schema::block::CHECKED`].
    ToDo,
    Quote,
    /// Fenced code; fence language lives in [`crate::schema::block::LANG`].
    Code,
    Divider,
    /// Block-level image; [`crate::schema::block::SRC`] + [`crate::schema::block::ALT`].
    Image,
}

impl BlockKind {
    pub const ALL: [BlockKind; 11] = [
        BlockKind::Paragraph,
        BlockKind::Heading1,
        BlockKind::Heading2,
        BlockKind::Heading3,
        BlockKind::BulletedListItem,
        BlockKind::NumberedListItem,
        BlockKind::ToDo,
        BlockKind::Quote,
        BlockKind::Code,
        BlockKind::Divider,
        BlockKind::Image,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            BlockKind::Paragraph => "paragraph",
            BlockKind::Heading1 => "heading_1",
            BlockKind::Heading2 => "heading_2",
            BlockKind::Heading3 => "heading_3",
            BlockKind::BulletedListItem => "bulleted_list_item",
            BlockKind::NumberedListItem => "numbered_list_item",
            BlockKind::ToDo => "todo",
            BlockKind::Quote => "quote",
            BlockKind::Code => "code",
            BlockKind::Divider => "divider",
            BlockKind::Image => "image",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "paragraph" | "text" => BlockKind::Paragraph,
            "heading_1" | "h1" => BlockKind::Heading1,
            "heading_2" | "h2" => BlockKind::Heading2,
            "heading_3" | "h3" => BlockKind::Heading3,
            "bulleted_list_item" | "bullet" => BlockKind::BulletedListItem,
            "numbered_list_item" | "number" => BlockKind::NumberedListItem,
            "todo" | "to_do" => BlockKind::ToDo,
            "quote" => BlockKind::Quote,
            "code" => BlockKind::Code,
            "divider" => BlockKind::Divider,
            "image" => BlockKind::Image,
            _ => return None,
        })
    }

    pub fn heading(level: u8) -> Self {
        match level {
            1 => BlockKind::Heading1,
            2 => BlockKind::Heading2,
            _ => BlockKind::Heading3,
        }
    }

    /// Does this kind carry rich text?
    pub fn has_text(self) -> bool {
        !matches!(self, BlockKind::Divider | BlockKind::Image)
    }

    /// Can this kind hold child blocks?
    pub fn can_nest(self) -> bool {
        !matches!(
            self,
            BlockKind::Code | BlockKind::Divider | BlockKind::Image | BlockKind::Quote
        )
    }

    pub fn is_list(self) -> bool {
        matches!(
            self,
            BlockKind::BulletedListItem | BlockKind::NumberedListItem | BlockKind::ToDo
        )
    }
}

/// Inline marks. `link` is the only mark with a payload.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Marks {
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub code: bool,
    pub link: Option<String>,
}

impl Marks {
    pub fn is_plain(&self) -> bool {
        !self.bold && !self.italic && !self.strike && !self.code && self.link.is_none()
    }
}

/// One inline mark to apply to a text range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mark {
    Bold,
    Italic,
    Strike,
    Code,
    Link(String),
}

impl Mark {
    pub fn key(&self) -> &'static str {
        match self {
            Mark::Bold => crate::schema::mark::BOLD,
            Mark::Italic => crate::schema::mark::ITALIC,
            Mark::Strike => crate::schema::mark::STRIKE,
            Mark::Code => crate::schema::mark::CODE,
            Mark::Link(_) => crate::schema::mark::LINK,
        }
    }
}

/// A run of text with one mark set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub marks: Marks,
}

/// Rich text as an ordered list of marked spans.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RichText {
    spans: Vec<Span>,
}

impl RichText {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn plain(text: impl Into<String>) -> Self {
        let text = text.into();
        let mut rt = Self::default();
        rt.push(&text, Marks::default());
        rt
    }

    pub fn spans(&self) -> &[Span] {
        &self.spans
    }

    /// Append `text` with `marks`, merging into the previous span when equal.
    pub fn push(&mut self, text: &str, marks: Marks) {
        if text.is_empty() {
            return;
        }
        match self.spans.last_mut() {
            Some(last) if last.marks == marks => last.text.push_str(text),
            _ => self.spans.push(Span {
                text: text.to_string(),
                marks,
            }),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.spans.iter().all(|s| s.text.is_empty())
    }

    /// Plain text with marks dropped — the search/plain projection.
    pub fn text(&self) -> String {
        let mut out = String::with_capacity(self.len_bytes());
        for span in &self.spans {
            out.push_str(&span.text);
        }
        out
    }

    pub fn len_chars(&self) -> usize {
        self.spans.iter().map(|s| s.text.chars().count()).sum()
    }

    pub fn len_bytes(&self) -> usize {
        self.spans.iter().map(|s| s.text.len()).sum()
    }

    /// Convert a character range into (span index, byte offset) walk order,
    /// used when applying marks to text that has already been split.
    pub fn spans_in_char_range(&self, start: usize, end: usize) -> Vec<(usize, usize, usize)> {
        let mut out = Vec::new();
        if end <= start {
            return out;
        }
        let mut cursor = 0usize;
        for (i, span) in self.spans.iter().enumerate() {
            let span_chars = span.text.chars().count();
            let span_start = cursor;
            let span_end = cursor + span_chars;
            cursor = span_end;
            if span_end <= start || span_start >= end {
                continue;
            }
            let local_start = start.saturating_sub(span_start);
            let local_end = end.min(span_end) - span_start;
            let byte_start: usize = span
                .text
                .char_indices()
                .nth(local_start)
                .map(|(b, _)| b)
                .unwrap_or(span.text.len());
            let byte_end: usize = span
                .text
                .char_indices()
                .nth(local_end)
                .map(|(b, _)| b)
                .unwrap_or(span.text.len());
            out.push((i, byte_start, byte_end));
        }
        out
    }
}

/// A block as read out of the document: identity, position and content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub id: BlockId,
    pub parent: Option<BlockId>,
    /// 0 for a top-level block, 1 for its children, and so on.
    pub depth: usize,
    pub kind: BlockKind,
    pub text: RichText,
    pub checked: bool,
    pub lang: Option<String>,
    pub src: Option<String>,
    pub alt: Option<String>,
}

impl Block {
    pub fn plain(&self) -> String {
        self.text.text()
    }
}
