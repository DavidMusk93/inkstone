//! Markdown is the projection, not the truth.
//!
//! Notes are CRDT documents; markdown exists so a note can be read, diffed,
//! exported, pasted into a chat, or imported from an existing vault. Rendering
//! is lossless for the block structure and for the inline marks this crate
//! models; parsing covers the same subset (see `docs/ARCHITECTURE.md`).

use crate::blocks::{Block, BlockKind, Mark, Marks, RichText};
use crate::doc::{NoteDoc, Result};

/// Render a note as markdown.
pub fn to_markdown(note: &NoteDoc) -> String {
    let mut out = String::new();
    let title = note.title();
    if !title.trim().is_empty() {
        out.push_str("# ");
        out.push_str(title.trim());
        out.push('\n');
    }
    render_blocks(&note.blocks(), &mut out);
    collapse_blank_runs(&out)
}

fn render_blocks(blocks: &[Block], out: &mut String) {
    let mut prev: Option<(usize, BlockKind)> = None;
    for block in blocks {
        // Consecutive list items at the same depth stay tight; everything else
        // gets a blank line. Rendering a tight list as a loose one changes what
        // the note looks like when it goes out as markdown.
        let tight = matches!(
            prev,
            Some((depth, kind)) if depth == block.depth && kind.is_list() && block.kind.is_list()
        );
        if !out.is_empty() && !tight {
            out.push('\n');
        }
        let indent = "  ".repeat(block.depth);
        for line in render_block(block).lines() {
            if line.is_empty() {
                out.push('\n');
            } else {
                out.push_str(&indent);
                out.push_str(line);
                out.push('\n');
            }
        }
        prev = Some((block.depth, block.kind));
    }
}

/// Markdown gives no meaning to a run of blank lines, so a run of empty
/// paragraph blocks must not become one. Fenced code keeps its blank lines: those
/// are content.
fn collapse_blank_runs(markdown: &str) -> String {
    let mut out = String::with_capacity(markdown.len());
    let mut in_fence = false;
    let mut blank = false;
    for line in markdown.split('\n') {
        let fence = line.trim_start().starts_with("```");
        if fence {
            in_fence = !in_fence;
        }
        let is_blank = line.trim().is_empty();
        if is_blank && !in_fence {
            if blank {
                continue;
            }
            blank = true;
            out.push('\n');
            continue;
        }
        blank = false;
        out.push_str(line);
        out.push('\n');
    }
    while out.ends_with('\n') {
        out.pop();
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

fn render_block(block: &Block) -> String {
    let inline = render_inline(&block.text);
    match block.kind {
        BlockKind::Paragraph => format!("{inline}\n"),
        BlockKind::Heading1 => format!("# {inline}\n"),
        BlockKind::Heading2 => format!("## {inline}\n"),
        BlockKind::Heading3 => format!("### {inline}\n"),
        BlockKind::BulletedListItem => format!("- {inline}\n"),
        BlockKind::NumberedListItem => format!("1. {inline}\n"),
        BlockKind::ToDo => format!("- [{}] {inline}\n", if block.checked { "x" } else { " " }),
        BlockKind::Quote => {
            let mut out = String::new();
            for line in inline.split('\n') {
                out.push_str("> ");
                out.push_str(line);
                out.push('\n');
            }
            out
        }
        BlockKind::Code => {
            let lang = block.lang.clone().unwrap_or_default();
            format!("```{lang}\n{}\n```\n", inline)
        }
        BlockKind::Divider => "---\n".to_string(),
        BlockKind::Image => {
            let src = block.src.clone().unwrap_or_default();
            let alt = block.alt.clone().unwrap_or_default();
            format!("![{alt}]({src})\n")
        }
    }
}

fn render_inline(text: &RichText) -> String {
    let mut out = String::new();
    for span in text.spans() {
        out.push_str(&render_span(span));
    }
    out
}

fn render_span(span: &crate::blocks::Span) -> String {
    let marks = &span.marks;
    let mut body = span.text.clone();
    if marks.code {
        body = format!("`{body}`");
    }
    if marks.strike {
        body = format!("~~{body}~~");
    }
    if marks.bold {
        body = format!("**{body}**");
    }
    if marks.italic {
        body = format!("*{body}*");
    }
    if let Some(url) = &marks.link {
        body = format!("[{body}]({url})");
    }
    body
}

/// Replace the note's content with the parsed markdown. The title is taken from
/// the leading `# ` line when present.
pub fn apply_markdown(note: &NoteDoc, markdown: &str) -> Result<()> {
    let (title, rest) = split_title(markdown);
    note.set_title(&title);
    note.clear_blocks();
    let blocks = parse_blocks(rest);
    let mut stack: Vec<(usize, crate::blocks::BlockId)> = Vec::new();
    for parsed in blocks {
        while let Some((depth, _)) = stack.last() {
            if *depth >= parsed.depth {
                stack.pop();
            } else {
                break;
            }
        }
        let parent = stack.last().map(|(_, id)| *id);
        let id = note.append_block(parent, parsed.kind, &parsed.text)?;
        if parsed.kind == BlockKind::ToDo && parsed.checked {
            note.set_checked(id, true)?;
        }
        if let Some(lang) = parsed.lang {
            note.set_code_lang(id, &lang)?;
        }
        if let Some((src, alt)) = parsed.image {
            note.set_image(id, &src, &alt)?;
        }
        let mut cursor = 0usize;
        for span in &parsed.spans {
            let chars = span.text.chars().count();
            if !span.marks.is_plain() {
                for mark in marks_to_vec(&span.marks) {
                    note.mark_block_text(id, cursor..cursor + chars, mark)?;
                }
            }
            cursor += chars;
        }
        if parsed.kind.can_nest() {
            stack.push((parsed.depth, id));
        }
    }
    Ok(())
}

fn split_title(markdown: &str) -> (String, &str) {
    let trimmed = markdown.trim_start_matches('\n');
    if let Some(first_line_end) = trimmed.find('\n') {
        let first = &trimmed[..first_line_end];
        if let Some(rest) = first.strip_prefix("# ") {
            return (rest.trim().to_string(), &trimmed[first_line_end + 1..]);
        }
    } else if let Some(rest) = trimmed.strip_prefix("# ") {
        return (rest.trim().to_string(), "");
    }
    (String::new(), markdown)
}

fn marks_to_vec(marks: &Marks) -> Vec<Mark> {
    let mut out = Vec::new();
    if marks.bold {
        out.push(Mark::Bold);
    }
    if marks.italic {
        out.push(Mark::Italic);
    }
    if marks.strike {
        out.push(Mark::Strike);
    }
    if marks.code {
        out.push(Mark::Code);
    }
    if let Some(url) = &marks.link {
        out.push(Mark::Link(url.clone()));
    }
    out
}

struct ParsedBlock {
    depth: usize,
    kind: BlockKind,
    text: String,
    spans: Vec<crate::blocks::Span>,
    checked: bool,
    lang: Option<String>,
    image: Option<(String, String)>,
}

fn parse_blocks(markdown: &str) -> Vec<ParsedBlock> {
    let mut out = Vec::new();
    let mut fence: Option<(usize, String)> = None;
    let mut code_body: Vec<String> = Vec::new();
    let mut code_lang = String::new();

    for raw in markdown.lines() {
        if let Some((depth, _)) = &fence {
            if raw.trim_start().starts_with("```") {
                out.push(ParsedBlock {
                    depth: *depth,
                    kind: BlockKind::Code,
                    text: code_body.join("\n"),
                    spans: RichText::plain(code_body.join("\n")).spans().to_vec(),
                    checked: false,
                    lang: Some(std::mem::take(&mut code_lang)),
                    image: None,
                });
                code_body.clear();
                fence = None;
                continue;
            }
            code_body.push(raw.to_string());
            continue;
        }

        let indent = raw.len() - raw.trim_start_matches(' ').len();
        let depth = indent / 2;
        let line = raw.trim_start();
        if line.is_empty() {
            continue;
        }
        if let Some(lang) = line.strip_prefix("```") {
            fence = Some((depth, lang.to_string()));
            code_lang = lang.trim().to_string();
            continue;
        }
        if line == "---" || line == "***" || line == "___" {
            out.push(simple(depth, BlockKind::Divider, ""));
            continue;
        }
        if let Some(rest) = line.strip_prefix("- [ ] ") {
            out.push(todo(depth, rest, false));
            continue;
        }
        if let Some(rest) = line
            .strip_prefix("- [x] ")
            .or_else(|| line.strip_prefix("- [X] "))
        {
            out.push(todo(depth, rest, true));
            continue;
        }
        if let Some(rest) = line
            .strip_prefix("- ")
            .or_else(|| line.strip_prefix("* "))
            .or_else(|| line.strip_prefix("+ "))
        {
            out.push(parsed(depth, BlockKind::BulletedListItem, rest));
            continue;
        }
        if let Some((number, rest)) = split_ordered_marker(line) {
            let _ = number;
            out.push(parsed(depth, BlockKind::NumberedListItem, rest));
            continue;
        }
        if let Some(rest) = line.strip_prefix('>') {
            out.push(parsed(depth, BlockKind::Quote, rest.trim_start()));
            continue;
        }
        if let Some((alt, src)) = parse_image(line) {
            let mut block = simple(depth, BlockKind::Image, "");
            block.image = Some((src, alt));
            out.push(block);
            continue;
        }
        if let Some(level) = heading_level(line) {
            let rest = line[level + 1..].trim_start();
            out.push(parsed(depth, BlockKind::heading(level as u8), rest));
            continue;
        }
        out.push(parsed(depth, BlockKind::Paragraph, line));
    }

    if let Some((depth, _)) = fence {
        // An unterminated fence is still code: never drop the user's text.
        let text = code_body.join("\n");
        out.push(ParsedBlock {
            depth,
            kind: BlockKind::Code,
            spans: RichText::plain(text.clone()).spans().to_vec(),
            text,
            checked: false,
            lang: Some(code_lang),
            image: None,
        });
    }
    out
}

fn split_ordered_marker(line: &str) -> Option<(String, &str)> {
    let dot = line.find(". ")?;
    let number = &line[..dot];
    if number.is_empty() || number.len() > 9 || !number.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some((number.to_string(), &line[dot + 2..]))
}

fn heading_level(line: &str) -> Option<usize> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = line.get(hashes..)?;
    if rest.starts_with(' ') {
        Some(hashes)
    } else {
        None
    }
}

fn parse_image(line: &str) -> Option<(String, String)> {
    let rest = line.strip_prefix("![")?;
    let close = rest.find("](")?;
    let alt = rest[..close].to_string();
    let after = &rest[close + 2..];
    let end = after.find(')')?;
    Some((alt, after[..end].to_string()))
}

fn simple(depth: usize, kind: BlockKind, text: &str) -> ParsedBlock {
    ParsedBlock {
        depth,
        kind,
        text: text.to_string(),
        spans: RichText::plain(text).spans().to_vec(),
        checked: false,
        lang: None,
        image: None,
    }
}

fn parsed(depth: usize, kind: BlockKind, text: &str) -> ParsedBlock {
    let spans = parse_inline(text);
    ParsedBlock {
        depth,
        kind,
        text: spans.text(),
        spans: spans.spans().to_vec(),
        checked: false,
        lang: None,
        image: None,
    }
}

fn todo(depth: usize, text: &str, checked: bool) -> ParsedBlock {
    ParsedBlock {
        checked,
        ..parsed(depth, BlockKind::ToDo, text)
    }
}

/// Parse the inline subset: `code`, `~~strike~~`, `**bold**`, `*italic*`,
/// `[text](url)`. Unmatched markers stay literal.
pub fn parse_inline(input: &str) -> RichText {
    let mut out = RichText::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0usize;
    let mut plain = String::new();

    macro_rules! flush {
        () => {
            if !plain.is_empty() {
                out.push(&plain, Marks::default());
                plain.clear();
            }
        };
    }

    while i < chars.len() {
        let c = chars[i];
        if c == '`' {
            if let Some(end) = find_char(&chars, i + 1, '`') {
                flush!();
                let text: String = chars[i + 1..end].iter().collect();
                let marks = Marks {
                    code: true,
                    ..Marks::default()
                };
                out.push(&text, marks);
                i = end + 1;
                continue;
            }
        }
        if c == '~' && chars.get(i + 1) == Some(&'~') {
            if let Some(end) = find_seq(&chars, i + 2, &['~', '~']) {
                flush!();
                let text: String = chars[i + 2..end].iter().collect();
                let marks = Marks {
                    strike: true,
                    ..Marks::default()
                };
                out.push(&text, marks);
                i = end + 2;
                continue;
            }
        }
        // Only `*` opens emphasis. `_` is deliberately not a delimiter: the
        // notes this engine imports are full of identifiers, URLs and paths
        // (`create_user`, `page_size`), and CommonMark's intraword rules would
        // still mangle pairs of them into italics.
        if c == '*' && chars.get(i + 1) == Some(&c) {
            if let Some(end) = find_seq(&chars, i + 2, &[c, c]) {
                flush!();
                let text: String = chars[i + 2..end].iter().collect();
                let marks = Marks {
                    bold: true,
                    ..Marks::default()
                };
                out.push(&text, marks);
                i = end + 2;
                continue;
            }
        }
        if c == '*' {
            if let Some(end) = find_char(&chars, i + 1, c) {
                if end > i + 1 {
                    flush!();
                    let text: String = chars[i + 1..end].iter().collect();
                    let marks = Marks {
                        italic: true,
                        ..Marks::default()
                    };
                    out.push(&text, marks);
                    i = end + 1;
                    continue;
                }
            }
        }
        if c == '[' {
            if let Some((text, url, end)) = parse_link(&chars, i) {
                flush!();
                let marks = Marks {
                    link: Some(url),
                    ..Marks::default()
                };
                out.push(&text, marks);
                i = end;
                continue;
            }
        }
        plain.push(c);
        i += 1;
    }
    flush!();
    out
}

fn parse_link(chars: &[char], start: usize) -> Option<(String, String, usize)> {
    let close = find_char(chars, start + 1, ']')?;
    if chars.get(close + 1) != Some(&'(') {
        return None;
    }
    let end = find_char(chars, close + 2, ')')?;
    let text: String = chars[start + 1..close].iter().collect();
    let url: String = chars[close + 2..end].iter().collect();
    Some((text, url, end + 1))
}

fn find_char(chars: &[char], from: usize, needle: char) -> Option<usize> {
    (from..chars.len()).find(|i| chars[*i] == needle)
}

fn find_seq(chars: &[char], from: usize, needle: &[char]) -> Option<usize> {
    if needle.is_empty() || chars.len() < needle.len() {
        return None;
    }
    (from..=chars.len() - needle.len()).find(|i| &chars[*i..*i + needle.len()] == needle)
}

#[cfg(test)]
mod underscore_tests {
    use super::*;

    #[test]
    fn underscores_stay_literal() {
        let text = "https://x/scm?create_user=1&page_size=10";
        let parsed = parse_inline(text);
        assert_eq!(
            parsed.text(),
            text,
            "identifiers and URLs keep their underscores"
        );
        assert!(parsed.spans().iter().all(|s| s.marks.is_plain()));
    }

    #[test]
    fn italics_use_asterisks_only() {
        let parsed = parse_inline("a *b* c");
        assert!(parsed
            .spans()
            .iter()
            .any(|s| s.marks.italic && s.text == "b"));
    }
}
