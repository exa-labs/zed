use crate::display_map::{
    BlockContext, BlockPlacement, BlockProperties, BlockStyle, Crease, CustomBlockId,
    FoldPlaceholder, HighlightKey, RenderBlock,
};
use crate::{Editor, ToggleMarkdownWysiwyg};
use gpui::{
    App, AppContext as _, Context, ElementId, Entity, FontStyle, FontWeight, HighlightStyle, Hsla,
    InteractiveElement, IntoElement, ParentElement, SharedString, StatefulInteractiveElement,
    Styled, Task, TextStyleRefinement, Window,
};
use multi_buffer::{Anchor, MultiBufferOffset, MultiBufferSnapshot, ToOffset};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use theme::ActiveTheme;
use util::ResultExt as _;
use collections::HashSet;
use std::any::TypeId;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Global flag that tracks whether WYSIWYG mode should be auto-enabled for
/// markdown files. When the user toggles WYSIWYG on, this is set to true;
/// when toggled off, set to false. New editors for markdown files check this
/// flag and auto-enable WYSIWYG if it is true.
static WYSIWYG_GLOBALLY_ENABLED: AtomicBool = AtomicBool::new(false);

struct WysiwygFoldTag;

const REPARSE_DEBOUNCE: Duration = Duration::from_millis(200);
const READABLE_LINE_LENGTH: u32 = 60;
/// Primary serif family for the rendered document, chosen per-OS.
///
/// GPUI's font resolver only consults a font's fallback list for missing
/// *glyphs*, not a missing *primary* family: if the primary family is absent
/// the text silently drops to the monospace buffer font. So the primary must
/// be a serif that ships with the host OS by default rather than a single
/// hardcoded name (a Linux-only face like "Liberation Serif" renders as
/// monospace on macOS, which is what made the document look like code).
fn wysiwyg_serif_family() -> SharedString {
    if cfg!(target_os = "macos") {
        SharedString::new_static("Palatino")
    } else if cfg!(target_os = "windows") {
        SharedString::new_static("Georgia")
    } else {
        SharedString::new_static("DejaVu Serif")
    }
}

/// Cross-platform serif chain used for glyph coverage when the primary family
/// lacks a specific glyph. Unknown families are ignored by the resolver.
fn wysiwyg_serif_fallbacks() -> gpui::FontFallbacks {
    gpui::FontFallbacks::from_fonts(vec![
        "Palatino".to_owned(),
        "Hoefler Text".to_owned(),
        "Georgia".to_owned(),
        "Charter".to_owned(),
        "DejaVu Serif".to_owned(),
        "Liberation Serif".to_owned(),
        "Times New Roman".to_owned(),
    ])
}

pub struct MarkdownWysiwygState {
    pub active: bool,
    reparse_task: Task<()>,
    references_task: Task<()>,
    /// Replace/Below blocks and the anchored buffer range each one renders.
    /// Anchors (not offsets) are stored so that edits elsewhere in the document
    /// shift a block's tracked range automatically; the diff in `apply_blocks`
    /// then keeps unchanged blocks in place instead of tearing them down and
    /// re-inserting them, which previously made the text below the cursor jump
    /// a moment after each edit.
    block_ids: Vec<(CustomBlockId, Range<Anchor>)>,
    references_block_ids: Vec<CustomBlockId>,
    pub cached_references: Vec<String>,
    /// References currently drawn in the "Linked Mentions" block. Used to make
    /// `apply_references_block` idempotent so cursor moves don't tear down and
    /// rebuild the block on every selection change.
    rendered_references: Vec<String>,
    /// The line range that was treated as "active" (revealed as raw markdown)
    /// during the last decoration update. Lets cursor moves update folds only
    /// for the lines whose active state changed instead of rebuilding every fold.
    previous_active_line_range: Option<Range<usize>>,
    /// Buffer rows covered by the newest selection at the last decoration
    /// update. Typing within a line keeps these rows constant, so this lets
    /// `on_selection_changed` skip the (expensive) full-document reparse on
    /// every keystroke and only reparse when the cursor actually changes lines.
    previous_active_rows: Option<Range<u32>>,
    previous_show_gutter: Option<bool>,
    previous_show_line_numbers: Option<Option<bool>>,
    previous_soft_wrap_override: Option<Option<language::language_settings::SoftWrap>>,
    /// Guard flag to prevent recursive cursor adjustment in on_selection_changed.
    adjusting_cursor: bool,
    /// Fingerprint of the document's markdown *structure* (marker kinds, counts,
    /// and lengths, with absolute offsets excluded) at the last decoration
    /// rebuild. Typing plain text shifts every offset after the cursor but does
    /// not change this fingerprint, so the debounced refresh can skip the
    /// O(document) fold/highlight/block rebuild entirely — the already-anchored
    /// folds, highlights, and blocks track the edit on their own. Rebuilding all
    /// folds on every keystroke (which tears down and re-wraps the whole display
    /// map) was the dominant source of typing lag on large documents.
    last_structure_fingerprint: Option<u64>,
}

impl MarkdownWysiwygState {
    pub fn new() -> Self {
        Self {
            active: false,
            reparse_task: Task::ready(()),
            references_task: Task::ready(()),
            block_ids: Vec::new(),
            adjusting_cursor: false,
            references_block_ids: Vec::new(),
            cached_references: Vec::new(),
            rendered_references: Vec::new(),
            previous_active_line_range: None,
            previous_active_rows: None,
            previous_show_gutter: None,
            previous_show_line_numbers: None,
            previous_soft_wrap_override: None,
            last_structure_fingerprint: None,
        }
    }
}

struct InlineDecoration {
    content_range: Range<usize>,
    kind: DecorationKind,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum DecorationKind {
    Bold,
    Italic,
    Strikethrough,
    InlineCode,
    Highlight,
}

struct HeadingDecoration {
    line_range: Range<usize>,
    level: u8,
}

struct TableDecoration {
    range: Range<usize>,
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
}

struct ImageDecoration {
    range: Range<usize>,
    url: String,
    width: Option<u32>,
    height: Option<u32>,
}

struct SyntaxMarker {
    range: Range<usize>,
}

#[allow(dead_code)]
struct WikilinkDecoration {
    range: Range<usize>,
    display_text: String,
}

struct ListItemDecoration {
    marker_range: Range<usize>,
}

struct ExternalLinkDecoration {
    text_range: Range<usize>,
}

struct TaskCheckboxDecoration {
    marker_range: Range<usize>,
    checked: bool,
}

struct BlockquoteDecoration {
    marker_range: Range<usize>,
}

struct CalloutDecoration {
    marker_range: Range<usize>,
    kind: String,
}

struct HorizontalRuleDecoration {
    range: Range<usize>,
}

struct BlockIdDecoration {
    range: Range<usize>,
}

struct MarkdownDecorations {
    inline_decorations: Vec<InlineDecoration>,
    headings: Vec<HeadingDecoration>,
    tables: Vec<TableDecoration>,
    images: Vec<ImageDecoration>,
    syntax_markers: Vec<SyntaxMarker>,
    wikilinks: Vec<WikilinkDecoration>,
    list_items: Vec<ListItemDecoration>,
    external_links: Vec<ExternalLinkDecoration>,
    task_checkboxes: Vec<TaskCheckboxDecoration>,
    blockquotes: Vec<BlockquoteDecoration>,
    callouts: Vec<CalloutDecoration>,
    horizontal_rules: Vec<HorizontalRuleDecoration>,
    ordered_list_markers: Vec<Range<usize>>,
    block_ids: Vec<BlockIdDecoration>,
}

impl MarkdownDecorations {
    /// Hash of the document structure that is invariant to absolute offset
    /// shifts. Two parses that differ only by text inserted or removed *within*
    /// existing runs (the overwhelmingly common typing case) hash equal,
    /// because only the kind, count, length, and rendered payload of each
    /// marker is mixed in — never an absolute buffer offset. When this matches
    /// the previously applied value the debounced refresh skips the full
    /// fold/highlight/block rebuild; the anchored decorations already track the
    /// edit, and the on-cursor-line content is handled by the cursor-move path.
    fn structure_fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();

        self.inline_decorations.len().hash(&mut hasher);
        for decoration in &self.inline_decorations {
            decoration.kind.hash(&mut hasher);
        }

        self.headings.len().hash(&mut hasher);
        for heading in &self.headings {
            heading.level.hash(&mut hasher);
        }

        self.tables.len().hash(&mut hasher);
        for table in &self.tables {
            table.headers.len().hash(&mut hasher);
            table.rows.len().hash(&mut hasher);
        }

        self.images.len().hash(&mut hasher);

        self.syntax_markers.len().hash(&mut hasher);
        for marker in &self.syntax_markers {
            (marker.range.end - marker.range.start).hash(&mut hasher);
        }

        self.list_items.len().hash(&mut hasher);
        self.external_links.len().hash(&mut hasher);

        self.task_checkboxes.len().hash(&mut hasher);
        for task in &self.task_checkboxes {
            task.checked.hash(&mut hasher);
        }

        self.blockquotes.len().hash(&mut hasher);

        self.callouts.len().hash(&mut hasher);
        for callout in &self.callouts {
            callout.kind.hash(&mut hasher);
        }

        self.horizontal_rules.len().hash(&mut hasher);

        self.ordered_list_markers.len().hash(&mut hasher);
        for marker in &self.ordered_list_markers {
            (marker.end - marker.start).hash(&mut hasher);
        }

        self.block_ids.len().hash(&mut hasher);

        hasher.finish()
    }
}

fn parse_options() -> Options {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_TASKLISTS);
    options
}

fn parse_image_dimensions(title: &str) -> (Option<u32>, Option<u32>) {
    let trimmed = title.trim();
    if trimmed.is_empty() {
        return (None, None);
    }
    if let Some(inner) = trimmed.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
        let parts: Vec<&str> = inner.split(',').collect();
        let width = parts.first().and_then(|s| s.trim().parse::<u32>().ok());
        let height = parts.get(1).and_then(|s| s.trim().parse::<u32>().ok());
        return (width, height);
    }
    if let Ok(width) = trimmed.parse::<u32>() {
        return (Some(width), None);
    }
    (None, None)
}

fn parse_wikilink_images(text: &str) -> Vec<ImageDecoration> {
    let mut results = Vec::new();
    let mut search_start = 0;
    while let Some(start) = text[search_start..].find("![[") {
        let absolute_start = search_start + start;
        let after_prefix = absolute_start + 3;
        if let Some(end_offset) = text[after_prefix..].find("]]") {
            let inner = &text[after_prefix..after_prefix + end_offset];
            let absolute_end = after_prefix + end_offset + 2;
            let (url, width) = if let Some(pipe_pos) = inner.find('|') {
                let path = inner[..pipe_pos].trim().to_string();
                let dimension = inner[pipe_pos + 1..].trim().parse::<u32>().ok();
                (path, dimension)
            } else {
                (inner.trim().to_string(), None)
            };
            results.push(ImageDecoration {
                range: absolute_start..absolute_end,
                url,
                width,
                height: None,
            });
            search_start = absolute_end;
        } else {
            break;
        }
    }
    results
}

fn parse_wikilinks(text: &str) -> Vec<WikilinkDecoration> {
    let mut results = Vec::new();
    let mut search_start = 0;
    while let Some(start) = text[search_start..].find("[[") {
        let absolute_start = search_start + start;
        if absolute_start > 0 && text.as_bytes().get(absolute_start.wrapping_sub(1)) == Some(&b'!') {
            search_start = absolute_start + 2;
            continue;
        }
        let after_prefix = absolute_start + 2;
        if let Some(end_offset) = text[after_prefix..].find("]]") {
            let inner = &text[after_prefix..after_prefix + end_offset];
            let absolute_end = after_prefix + end_offset + 2;
            let display_text = if let Some(pipe_pos) = inner.find('|') {
                inner[pipe_pos + 1..].trim().to_string()
            } else {
                inner.trim().to_string()
            };
            results.push(WikilinkDecoration {
                range: absolute_start..absolute_end,
                display_text,
            });
            search_start = absolute_end;
        } else {
            break;
        }
    }
    results
}

fn parse_list_items(text: &str) -> Vec<ListItemDecoration> {
    let mut results = Vec::new();
    for (line_start, _line) in text.match_indices('\n') {
        let content_start = line_start + 1;
        let rest = &text[content_start..];
        let trimmed = rest.trim_start();
        let indent = rest.len() - trimmed.len();
        if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
            let marker_start = content_start + indent;
            let marker_end = marker_start + 2;
            results.push(ListItemDecoration {
                marker_range: marker_start..marker_end,
            });
        }
    }
    if text.starts_with("- ") || text.starts_with("* ") {
        results.push(ListItemDecoration {
            marker_range: 0..2,
        });
    } else {
        let trimmed = text.trim_start();
        let indent = text.len() - trimmed.len();
        if indent > 0 && (trimmed.starts_with("- ") || trimmed.starts_with("* ")) {
            results.push(ListItemDecoration {
                marker_range: indent..indent + 2,
            });
        }
    }
    results
}

fn parse_highlights(text: &str) -> (Vec<InlineDecoration>, Vec<SyntaxMarker>) {
    let mut decorations = Vec::new();
    let mut markers = Vec::new();
    let mut search_start = 0;
    while let Some(start) = text[search_start..].find("==") {
        let absolute_start = search_start + start;
        let content_start = absolute_start + 2;
        let Some(end_offset) = text[content_start..].find("==") else {
            break;
        };
        if end_offset == 0 {
            search_start = content_start + 2;
            continue;
        }
        let content_end = content_start + end_offset;
        let absolute_end = content_end + 2;
        if text[content_start..content_end].contains('\n') {
            search_start = content_start;
            continue;
        }
        markers.push(SyntaxMarker {
            range: absolute_start..content_start,
        });
        markers.push(SyntaxMarker {
            range: content_end..absolute_end,
        });
        decorations.push(InlineDecoration {
            content_range: content_start..content_end,
            kind: DecorationKind::Highlight,
        });
        search_start = absolute_end;
    }
    (decorations, markers)
}

fn callout_kind(rest: &str) -> Option<(usize, String)> {
    let inner = rest.strip_prefix("[!")?;
    let close = inner.find(']')?;
    let kind = inner[..close].trim().to_lowercase();
    if kind.is_empty() || !kind.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return None;
    }
    Some((2 + close + 1, kind))
}

struct LineDecorations {
    task_checkboxes: Vec<TaskCheckboxDecoration>,
    blockquotes: Vec<BlockquoteDecoration>,
    callouts: Vec<CalloutDecoration>,
    ordered_list_markers: Vec<Range<usize>>,
}

fn parse_line_decorations(text: &str) -> LineDecorations {
    let mut task_checkboxes = Vec::new();
    let mut blockquotes = Vec::new();
    let mut callouts = Vec::new();
    let mut ordered_list_markers = Vec::new();

    let mut line_start = 0;
    for line in text.split_inclusive('\n') {
        let line_content = line.trim_end_matches('\n');
        let trimmed = line_content.trim_start();
        let indent = line_content.len() - trimmed.len();
        let content_start = line_start + indent;

        if trimmed.starts_with("> ") || trimmed == ">" {
            let marker_len = if trimmed.starts_with("> ") { 2 } else { 1 };
            let rest = &trimmed[marker_len..];
            if let Some((callout_len, kind)) = callout_kind(rest) {
                let mut marker_end = content_start + marker_len + callout_len;
                if text.as_bytes().get(marker_end) == Some(&b' ') {
                    marker_end += 1;
                }
                callouts.push(CalloutDecoration {
                    marker_range: content_start..marker_end,
                    kind,
                });
            } else {
                blockquotes.push(BlockquoteDecoration {
                    marker_range: content_start..content_start + marker_len,
                });
            }
        } else if let Some(rest) = trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* "))
        {
            let checked = if rest.starts_with("[ ] ") || rest == "[ ]" {
                Some(false)
            } else if rest.starts_with("[x] ") || rest.starts_with("[X] ") || rest == "[x]" || rest == "[X]" {
                Some(true)
            } else {
                None
            };
            if let Some(checked) = checked {
                task_checkboxes.push(TaskCheckboxDecoration {
                    marker_range: content_start..content_start + 5,
                    checked,
                });
            }
        } else {
            let digit_count = trimmed.chars().take_while(|c| c.is_ascii_digit()).count();
            if digit_count > 0 {
                let after_digits = &trimmed[digit_count..];
                if (after_digits.starts_with(". ") || after_digits.starts_with(") "))
                    && trimmed.len() > digit_count + 2
                {
                    ordered_list_markers
                        .push(content_start..content_start + digit_count + 1);
                }
            }
        }

        line_start += line.len();
    }

    LineDecorations {
        task_checkboxes,
        blockquotes,
        callouts,
        ordered_list_markers,
    }
}

fn parse_block_ids(text: &str) -> Vec<BlockIdDecoration> {
    let mut results = Vec::new();
    let mut line_start = 0;
    for line in text.split_inclusive('\n') {
        let line_content = line.trim_end_matches('\n');
        let trimmed_end = line_content.trim_end();
        if let Some(caret_pos) = trimmed_end.rfind('^') {
            if caret_pos > 0 {
                let before_caret = &trimmed_end[..caret_pos];
                if before_caret.ends_with(char::is_whitespace) {
                    let after_caret = &trimmed_end[caret_pos + 1..];
                    if !after_caret.is_empty()
                        && after_caret
                            .chars()
                            .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
                    {
                        let whitespace_start = before_caret.trim_end().len();
                        let block_id_start = line_start + whitespace_start;
                        let block_id_end = line_start + trimmed_end.len();
                        results.push(BlockIdDecoration {
                            range: block_id_start..block_id_end,
                        });
                    }
                }
            }
        }
        line_start += line.len();
    }
    results
}

fn parse_markdown_decorations(text: &str) -> MarkdownDecorations {
    let parser = Parser::new_ext(text, parse_options());

    let (mut inline_decorations, mut syntax_markers) = parse_highlights(text);
    let mut headings = Vec::new();
    let mut tables = Vec::new();
    let mut images: Vec<ImageDecoration> = parse_wikilink_images(text);
    let wikilinks = parse_wikilinks(text);
    let line_decorations = parse_line_decorations(text);
    let block_ids = parse_block_ids(text);
    let list_items: Vec<ListItemDecoration> = parse_list_items(text)
        .into_iter()
        .filter(|item| {
            !line_decorations
                .task_checkboxes
                .iter()
                .any(|task| task.marker_range.start == item.marker_range.start)
        })
        .collect();
    let mut external_links = Vec::new();
    let mut horizontal_rules = Vec::new();

    let mut bold_start: Option<usize> = None;
    let mut italic_start: Option<usize> = None;
    let mut strikethrough_start: Option<usize> = None;
    let mut heading_start: Option<(u8, usize)> = None;

    let mut table_start: Option<usize> = None;
    let mut table_headers: Vec<String> = Vec::new();
    let mut table_rows: Vec<Vec<String>> = Vec::new();
    let mut current_row: Vec<String> = Vec::new();
    let mut current_cell = String::new();
    let mut in_table_head = false;
    let mut in_table_cell = false;

    for (event, range) in parser.into_offset_iter() {
        match event {
            Event::Start(Tag::Strong) => {
                bold_start = Some(range.start);
                syntax_markers.push(SyntaxMarker {
                    range: range.start..range.start + 2,
                });
            }
            Event::End(TagEnd::Strong) => {
                if let Some(start) = bold_start.take() {
                    syntax_markers.push(SyntaxMarker {
                        range: range.end - 2..range.end,
                    });
                    inline_decorations.push(InlineDecoration {
                        content_range: start + 2..range.end - 2,
                        kind: DecorationKind::Bold,
                    });
                }
            }
            Event::Start(Tag::Emphasis) => {
                italic_start = Some(range.start);
                syntax_markers.push(SyntaxMarker {
                    range: range.start..range.start + 1,
                });
            }
            Event::End(TagEnd::Emphasis) => {
                if let Some(start) = italic_start.take() {
                    syntax_markers.push(SyntaxMarker {
                        range: range.end - 1..range.end,
                    });
                    inline_decorations.push(InlineDecoration {
                        content_range: start + 1..range.end - 1,
                        kind: DecorationKind::Italic,
                    });
                }
            }
            Event::Start(Tag::Strikethrough) => {
                strikethrough_start = Some(range.start);
                syntax_markers.push(SyntaxMarker {
                    range: range.start..range.start + 2,
                });
            }
            Event::End(TagEnd::Strikethrough) => {
                if let Some(start) = strikethrough_start.take() {
                    syntax_markers.push(SyntaxMarker {
                        range: range.end - 2..range.end,
                    });
                    inline_decorations.push(InlineDecoration {
                        content_range: start + 2..range.end - 2,
                        kind: DecorationKind::Strikethrough,
                    });
                }
            }
            Event::Code(code_text) => {
                let code_str = code_text.as_ref();
                let full_start = range.start;
                let full_end = range.end;
                if full_end > full_start + code_str.len() {
                    let backtick_len = (full_end - full_start - code_str.len()) / 2;
                    syntax_markers.push(SyntaxMarker {
                        range: full_start..full_start + backtick_len,
                    });
                    syntax_markers.push(SyntaxMarker {
                        range: full_end - backtick_len..full_end,
                    });
                    inline_decorations.push(InlineDecoration {
                        content_range: full_start + backtick_len..full_end - backtick_len,
                        kind: DecorationKind::InlineCode,
                    });
                } else {
                    inline_decorations.push(InlineDecoration {
                        content_range: full_start..full_end,
                        kind: DecorationKind::InlineCode,
                    });
                }
            }
            Event::Start(Tag::Heading { level, .. }) => {
                heading_start = Some((level as u8, range.start));
                let hash_count = level as usize;
                let marker_end = range.start + hash_count;
                let space_end = if text.as_bytes().get(marker_end) == Some(&b' ') {
                    marker_end + 1
                } else {
                    marker_end
                };
                syntax_markers.push(SyntaxMarker {
                    range: range.start..space_end,
                });
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some((level, start)) = heading_start.take() {
                    // The heading event range includes the trailing newline. A
                    // Replace block built from it would extend onto the next
                    // line's display row, so the block map would merge a run of
                    // consecutive headings into a single block (only the first
                    // renders). Trim the trailing line break so each heading's
                    // range stays on its own row.
                    let mut end = range.end.min(text.len());
                    while end > start
                        && matches!(text.as_bytes().get(end - 1), Some(b'\n') | Some(b'\r'))
                    {
                        end -= 1;
                    }
                    headings.push(HeadingDecoration {
                        line_range: start..end,
                        level,
                    });
                }
            }
            Event::Start(Tag::Link { .. }) => {
                let link_text = &text[range.start..range.end.min(text.len())];
                if link_text.starts_with('[') {
                    if let Some(text_end_relative) = link_text.rfind("](") {
                        let text_start = range.start + 1;
                        let text_end = range.start + text_end_relative;
                        if text_end > text_start {
                            syntax_markers.push(SyntaxMarker {
                                range: range.start..text_start,
                            });
                            syntax_markers.push(SyntaxMarker {
                                range: text_end..range.end,
                            });
                            external_links.push(ExternalLinkDecoration {
                                text_range: text_start..text_end,
                            });
                        }
                    }
                }
            }
            Event::Rule => {
                let mut end = range.end;
                while end > range.start && text.as_bytes().get(end - 1) == Some(&b'\n') {
                    end -= 1;
                }
                if end > range.start {
                    horizontal_rules.push(HorizontalRuleDecoration {
                        range: range.start..end,
                    });
                }
            }
            Event::Start(Tag::Image { dest_url, title, .. }) => {
                let url = dest_url.to_string();
                let mut end = range.end;
                if let Some(pos) = text[range.start..].find(')') {
                    end = range.start + pos + 1;
                }
                let (width, height) = parse_image_dimensions(&title);
                images.push(ImageDecoration {
                    range: range.start..end,
                    url,
                    width,
                    height,
                });
            }
            Event::Start(Tag::Table(_)) => {
                table_start = Some(range.start);
                table_headers.clear();
                table_rows.clear();
            }
            Event::End(TagEnd::Table) => {
                if let Some(start) = table_start.take() {
                    tables.push(TableDecoration {
                        range: start..range.end,
                        headers: table_headers.clone(),
                        rows: table_rows.clone(),
                    });
                }
            }
            Event::Start(Tag::TableHead) => {
                in_table_head = true;
                current_row.clear();
            }
            Event::End(TagEnd::TableHead) => {
                in_table_head = false;
                table_headers = current_row.clone();
                current_row.clear();
            }
            Event::Start(Tag::TableRow) => {
                current_row.clear();
            }
            Event::End(TagEnd::TableRow) => {
                if !in_table_head {
                    table_rows.push(current_row.clone());
                }
                current_row.clear();
            }
            Event::Start(Tag::TableCell) => {
                in_table_cell = true;
                current_cell.clear();
            }
            Event::End(TagEnd::TableCell) => {
                in_table_cell = false;
                current_row.push(current_cell.clone());
                current_cell.clear();
            }
            Event::Text(text_content) => {
                if in_table_cell {
                    current_cell.push_str(text_content.as_ref());
                }
            }
            _ => {}
        }
    }

    for wikilink in &wikilinks {
        syntax_markers.push(SyntaxMarker {
            range: wikilink.range.start..wikilink.range.start + 2,
        });
        syntax_markers.push(SyntaxMarker {
            range: wikilink.range.end - 2..wikilink.range.end,
        });
        if let Some(pipe_pos) = text[wikilink.range.start + 2..wikilink.range.end - 2].find('|') {
            let link_end = wikilink.range.start + 2 + pipe_pos + 1;
            syntax_markers.push(SyntaxMarker {
                range: wikilink.range.start + 2..link_end,
            });
        }
    }


    MarkdownDecorations {
        inline_decorations,
        headings,
        tables,
        images,
        syntax_markers,
        wikilinks,
        list_items,
        external_links,
        task_checkboxes: line_decorations.task_checkboxes,
        blockquotes: line_decorations.blockquotes,
        callouts: line_decorations.callouts,
        horizontal_rules,
        ordered_list_markers: line_decorations.ordered_list_markers,
        block_ids,
    }
}


fn cursor_offset(editor: &Editor, cx: &mut Context<Editor>) -> usize {
    let anchor = editor.selections.newest_anchor();
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let offset: MultiBufferOffset = anchor.head().to_offset(&snapshot);
    offset.0
}

/// Returns the full range of lines covered by the newest selection.
/// For a collapsed cursor this is just the cursor line; for a multi-line
/// selection it spans from the first selected line start to the last selected line end.
fn selection_line_range(editor: &Editor, text: &str, cx: &mut Context<Editor>) -> Range<usize> {
    let anchor = editor.selections.newest_anchor();
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let head_offset: usize = anchor.head().to_offset(&snapshot).0;
    let tail_offset: usize = anchor.tail().to_offset(&snapshot).0;
    let selection_start = head_offset.min(tail_offset);
    let selection_end = head_offset.max(tail_offset);

    let start_line = cursor_line_range(text, selection_start);
    let end_line = cursor_line_range(text, selection_end);
    start_line.start..end_line.end
}

#[allow(dead_code)]
fn range_contains_cursor(range: &Range<usize>, cursor: usize) -> bool {
    cursor >= range.start && cursor <= range.end
}

fn cursor_line_range(text: &str, cursor: usize) -> Range<usize> {
    let cursor = cursor.min(text.len());
    let line_start = text[..cursor].rfind('\n').map_or(0, |pos| pos + 1);
    let line_end = text[cursor..].find('\n').map_or(text.len(), |pos| cursor + pos);
    line_start..line_end
}

fn range_on_cursor_line(range: &Range<usize>, cursor_line: &Range<usize>) -> bool {
    range.start < cursor_line.end && range.end > cursor_line.start
}

impl Editor {
    pub fn toggle_markdown_wysiwyg(
        &mut self,
        _: &ToggleMarkdownWysiwyg,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.markdown_wysiwyg_state.active {
            clear_wysiwyg_decorations(self, cx);
            if let Some(show_gutter) = self.markdown_wysiwyg_state.previous_show_gutter {
                self.set_show_gutter(show_gutter, cx);
            }
            if let Some(show_line_numbers) = self.markdown_wysiwyg_state.previous_show_line_numbers
            {
                self.show_line_numbers = show_line_numbers;
            }
            if let Some(soft_wrap) = self.markdown_wysiwyg_state.previous_soft_wrap_override.take()
            {
                self.soft_wrap_mode_override = soft_wrap;
            }
            self.preferred_line_length_override = None;
            self.set_text_style_refinement(TextStyleRefinement::default());
            self.style = None;
            self.display_map
                .update(cx, |display_map, cx| display_map.set_hang_indent(false, cx));
            self.markdown_wysiwyg_state.active = false;
            WYSIWYG_GLOBALLY_ENABLED.store(false, Ordering::SeqCst);
            cx.notify();
        } else {
            self.markdown_wysiwyg_state.previous_show_gutter = Some(self.show_gutter);
            self.markdown_wysiwyg_state.previous_show_line_numbers = Some(self.show_line_numbers);
            self.markdown_wysiwyg_state.previous_soft_wrap_override =
                Some(self.soft_wrap_mode_override);

            self.set_text_style_refinement(TextStyleRefinement {
                font_family: Some(wysiwyg_serif_family()),
                font_fallbacks: Some(wysiwyg_serif_fallbacks()),
                ..Default::default()
            });
            self.style = None;

            self.set_show_gutter(false, cx);
            self.soft_wrap_mode_override =
                Some(language::language_settings::SoftWrap::Bounded);
            self.preferred_line_length_override = Some(READABLE_LINE_LENGTH);
            self.display_map
                .update(cx, |display_map, cx| display_map.set_hang_indent(true, cx));
            self.markdown_wysiwyg_state.active = true;
            WYSIWYG_GLOBALLY_ENABLED.store(true, Ordering::SeqCst);
            refresh_wysiwyg_decorations(self, cx);
            fetch_references(self, cx);
        }
    }

    /// Called after editor construction to auto-enable WYSIWYG mode if the
    /// global flag is set and the current file is a markdown file.
    /// Uses `defer_in` so the toggle runs after the editor is fully mounted,
    /// avoiding issues where decorations applied during `new_internal` get
    /// cleared by later initialisation steps.
    pub fn maybe_auto_enable_wysiwyg(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !WYSIWYG_GLOBALLY_ENABLED.load(Ordering::SeqCst) {
            return;
        }
        cx.defer_in(window, |editor, window, cx| {
            if editor.markdown_wysiwyg_state.active {
                return;
            }
            if !WYSIWYG_GLOBALLY_ENABLED.load(Ordering::SeqCst) {
                return;
            }
            if !editor.is_markdown_file(cx) {
                return;
            }
            editor.toggle_markdown_wysiwyg(&ToggleMarkdownWysiwyg, window, cx);
        });
    }

    fn is_markdown_file(&self, cx: &Context<Self>) -> bool {
        let Some(singleton_buffer) = self.buffer.read(cx).as_singleton() else {
            return false;
        };
        let buffer = singleton_buffer.read(cx);
        if let Some(file) = buffer.file() {
            if let Some(ext) = file.path().extension() {
                let ext = ext.to_lowercase();
                return ext == "md" || ext == "markdown" || ext == "mdx";
            }
        }
        false
    }
}

pub fn schedule_wysiwyg_refresh(editor: &mut Editor, cx: &mut Context<Editor>) {
    if !editor.markdown_wysiwyg_state.active {
        return;
    }

    editor.markdown_wysiwyg_state.reparse_task = cx.spawn(async move |editor, cx| {
        cx.background_executor().timer(REPARSE_DEBOUNCE).await;

        // Snapshot the text on the foreground (a cheap rope copy), then run the
        // markdown parse on a background thread. Parsing the whole buffer on the
        // UI thread on every edit was a large part of the per-keystroke cost.
        let Ok(text) =
            editor.read_with(cx, |editor, cx| editor.buffer().read(cx).snapshot(cx).text())
        else {
            return;
        };

        let (decorations, fingerprint, parsed_len) = cx
            .background_spawn(async move {
                let decorations = parse_markdown_decorations(&text);
                let fingerprint = decorations.structure_fingerprint();
                let parsed_len = text.len();
                (decorations, fingerprint, parsed_len)
            })
            .await;

        editor
            .update(cx, |editor, cx| {
                if !editor.markdown_wysiwyg_state.active {
                    return;
                }
                let snapshot = editor.buffer().read(cx).snapshot(cx);
                // If the buffer advanced between the background parse and now
                // (a rare race; an edit normally cancels this task), the parsed
                // offsets are stale, so fall back to a fresh synchronous rebuild.
                if snapshot.len().0 != parsed_len {
                    refresh_wysiwyg_decorations(editor, cx);
                    return;
                }
                // The fold/highlight/block set only depends on the document
                // *structure*. When that is unchanged (plain typing within an
                // existing run), the anchored decorations already tracked the
                // edit, so the expensive rebuild is pure churn — skip it.
                if editor.markdown_wysiwyg_state.last_structure_fingerprint == Some(fingerprint) {
                    return;
                }
                let text = snapshot.text();
                apply_wysiwyg_decorations(editor, &snapshot, &text, &decorations, cx);
                editor.markdown_wysiwyg_state.last_structure_fingerprint = Some(fingerprint);
            })
            .ok();
    });
}

/// Full rebuild of all WYSIWYG decorations. Removes and re-creates every fold
/// and re-diffs every block. Used when the document text changes or WYSIWYG is
/// toggled on. Cursor moves use `refresh_active_line_decorations` instead, which
/// only touches the lines whose active state changed.
fn refresh_wysiwyg_decorations(editor: &mut Editor, cx: &mut Context<Editor>) {
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let text = snapshot.text();
    let decorations = parse_markdown_decorations(&text);
    apply_wysiwyg_decorations(editor, &snapshot, &text, &decorations, cx);
    editor.markdown_wysiwyg_state.last_structure_fingerprint =
        Some(decorations.structure_fingerprint());
}

/// Applies a freshly parsed decoration set: re-highlights, rebuilds every fold,
/// and re-diffs every block against the given snapshot. `text` must be the text
/// of `snapshot` and `decorations` must have been parsed from it.
fn apply_wysiwyg_decorations(
    editor: &mut Editor,
    snapshot: &MultiBufferSnapshot,
    text: &str,
    decorations: &MarkdownDecorations,
    cx: &mut Context<Editor>,
) {
    let cursor = cursor_offset(editor, cx);
    // Use the full selection range (covers all lines in a multi-line selection)
    let active_line_range = selection_line_range(editor, text, cx);

    apply_highlights(editor, snapshot, decorations, cursor, &active_line_range, cx);
    remove_stale_folds(editor, cx);
    apply_marker_folds(
        editor,
        snapshot,
        decorations,
        cursor,
        &active_line_range,
        None,
        cx,
    );
    apply_blocks(editor, snapshot, decorations, &active_line_range, cx);
    editor.markdown_wysiwyg_state.previous_active_line_range = Some(active_line_range);
}

/// Incremental update for cursor moves. Only the line that gained or lost
/// "active" status changes, so this re-folds the markers on the line the cursor
/// left and reveals the markers on the line it entered, instead of tearing down
/// and rebuilding every fold (which caused transient layout flicker during
/// rapid navigation). Does nothing when the active line is unchanged.
fn refresh_active_line_decorations(editor: &mut Editor, cx: &mut Context<Editor>) {
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let text = snapshot.text();
    let cursor = cursor_offset(editor, cx);
    let active_line_range = selection_line_range(editor, &text, cx);

    let previous_active_line_range =
        editor.markdown_wysiwyg_state.previous_active_line_range.clone();
    if previous_active_line_range.as_ref() == Some(&active_line_range) {
        return;
    }

    let decorations = parse_markdown_decorations(&text);

    apply_highlights(editor, &snapshot, &decorations, cursor, &active_line_range, cx);

    // Re-fold the markers on the line the cursor just left so it renders again.
    if let Some(previous) = previous_active_line_range.as_ref() {
        apply_marker_folds(
            editor,
            &snapshot,
            &decorations,
            cursor,
            &active_line_range,
            Some(previous),
            cx,
        );
    }

    // Reveal the newly active line by removing the folds that intersect it.
    let active_offset_range =
        MultiBufferOffset(active_line_range.start)..MultiBufferOffset(active_line_range.end);
    editor.remove_folds_with_type(
        &[active_offset_range],
        TypeId::of::<WysiwygFoldTag>(),
        false,
        cx,
    );

    // Blocks are smart-diffed, so this only reveals/hides the heading, table,
    // image, or rule blocks on the lines whose active state changed.
    apply_blocks(editor, &snapshot, &decorations, &active_line_range, cx);

    editor.markdown_wysiwyg_state.previous_active_line_range = Some(active_line_range);
}

fn apply_highlights(
    editor: &mut Editor,
    snapshot: &MultiBufferSnapshot,
    decorations: &MarkdownDecorations,
    _cursor: usize,
    cursor_line: &Range<usize>,
    cx: &mut Context<Editor>,
) {
    let foreground = cx.theme().colors().editor_foreground;
    let block_ranges = collect_block_replaced_ranges(decorations, cursor_line);

    let mut bold_ranges = Vec::new();
    let mut italic_ranges = Vec::new();
    let mut strikethrough_ranges = Vec::new();
    let mut code_ranges = Vec::new();
    let mut highlight_ranges = Vec::new();

    for decoration in &decorations.inline_decorations {
        let start = snapshot.anchor_before(MultiBufferOffset(decoration.content_range.start));
        let end = snapshot.anchor_after(MultiBufferOffset(decoration.content_range.end));
        let range = start..end;

        match decoration.kind {
            DecorationKind::Bold => bold_ranges.push(range),
            DecorationKind::Italic => italic_ranges.push(range),
            DecorationKind::Strikethrough => strikethrough_ranges.push(range),
            DecorationKind::InlineCode => {
                if !marker_overlaps_block(&decoration.content_range, &block_ranges) {
                    code_ranges.push(range);
                }
            }
            DecorationKind::Highlight => {
                if !marker_overlaps_block(&decoration.content_range, &block_ranges) {
                    highlight_ranges.push(range);
                }
            }
        }
    }

    let mut external_link_ranges = Vec::new();
    for link in &decorations.external_links {
        if !marker_overlaps_block(&link.text_range, &block_ranges) {
            let start = snapshot.anchor_before(MultiBufferOffset(link.text_range.start));
            let end = snapshot.anchor_after(MultiBufferOffset(link.text_range.end));
            external_link_ranges.push(start..end);
        }
    }

    let mut ordered_marker_ranges = Vec::new();
    for marker in &decorations.ordered_list_markers {
        if !marker_overlaps_block(marker, &block_ranges) {
            let start = snapshot.anchor_before(MultiBufferOffset(marker.start));
            let end = snapshot.anchor_after(MultiBufferOffset(marker.end));
            ordered_marker_ranges.push(start..end);
        }
    }

    let mut heading_ranges = Vec::new();
    for heading in &decorations.headings {
        let start = snapshot.anchor_before(MultiBufferOffset(heading.line_range.start));
        let end = snapshot.anchor_after(MultiBufferOffset(heading.line_range.end));
        heading_ranges.push(start..end);
    }

    let marker_ranges: Vec<Range<multi_buffer::Anchor>> = Vec::new();

    set_or_clear_highlight(
        editor,
        HighlightKey::MarkdownWysiwygBold,
        bold_ranges,
        HighlightStyle {
            color: Some(foreground),
            font_weight: Some(FontWeight::BOLD),
            ..Default::default()
        },
        cx,
    );

    set_or_clear_highlight(
        editor,
        HighlightKey::MarkdownWysiwygItalic,
        italic_ranges,
        HighlightStyle {
            color: Some(foreground),
            font_style: Some(FontStyle::Italic),
            ..Default::default()
        },
        cx,
    );

    set_or_clear_highlight(
        editor,
        HighlightKey::MarkdownWysiwygStrikethrough,
        strikethrough_ranges,
        HighlightStyle {
            color: Some(foreground),
            strikethrough: Some(gpui::StrikethroughStyle {
                thickness: gpui::px(1.0),
                ..Default::default()
            }),
            ..Default::default()
        },
        cx,
    );

    set_or_clear_highlight(
        editor,
        HighlightKey::MarkdownWysiwygCode,
        code_ranges,
        HighlightStyle {
            color: Some(foreground),
            background_color: Some(Hsla {
                h: 0.0,
                s: 0.0,
                l: 0.2,
                a: 0.08,
            }),
            ..Default::default()
        },
        cx,
    );

    set_or_clear_highlight(
        editor,
        HighlightKey::MarkdownWysiwygHighlight,
        highlight_ranges,
        HighlightStyle {
            color: Some(Hsla {
                h: 0.13,
                s: 0.9,
                l: 0.25,
                a: 1.0,
            }),
            background_color: Some(Hsla {
                h: 0.13,
                s: 0.95,
                l: 0.6,
                a: 0.5,
            }),
            ..Default::default()
        },
        cx,
    );

    set_or_clear_highlight(
        editor,
        HighlightKey::MarkdownWysiwygExternalLink,
        external_link_ranges,
        HighlightStyle {
            color: Some(Hsla {
                h: 0.58,
                s: 0.6,
                l: 0.55,
                a: 1.0,
            }),
            underline: Some(gpui::UnderlineStyle {
                thickness: gpui::px(1.0),
                color: Some(Hsla {
                    h: 0.58,
                    s: 0.6,
                    l: 0.55,
                    a: 0.6,
                }),
                wavy: false,
            }),
            ..Default::default()
        },
        cx,
    );

    set_or_clear_highlight(
        editor,
        HighlightKey::MarkdownWysiwygListMarker,
        ordered_marker_ranges,
        HighlightStyle {
            color: Some(Hsla {
                h: 0.6,
                s: 0.3,
                l: 0.55,
                a: 1.0,
            }),
            font_weight: Some(FontWeight::SEMIBOLD),
            ..Default::default()
        },
        cx,
    );

    set_or_clear_highlight(
        editor,
        HighlightKey::MarkdownWysiwygHeading,
        heading_ranges,
        HighlightStyle {
            color: Some(foreground),
            font_weight: Some(FontWeight::BOLD),
            ..Default::default()
        },
        cx,
    );

    // Collect diagnostics for the full buffer to detect unresolved links
    let all_diagnostics: Vec<(Range<usize>, String)> = snapshot
        .diagnostics_in_range::<MultiBufferOffset>(MultiBufferOffset(0)..MultiBufferOffset(snapshot.len().0))
        .map(|entry| (entry.range.start.0..entry.range.end.0, entry.diagnostic.message.clone()))
        .collect();

    let mut wikilink_ranges = Vec::new();
    let mut unresolved_wikilink_ranges = Vec::new();
    for wikilink in &decorations.wikilinks {
        if !marker_overlaps_block(&wikilink.range, &block_ranges)
            && !range_on_cursor_line(&wikilink.range, cursor_line)
        {
            let content_start = wikilink.range.start + 2;
            let content_end = wikilink.range.end - 2;
            let inner = &snapshot.text()[content_start..content_end];
            let display_start = if let Some(pipe_pos) = inner.find('|') {
                content_start + pipe_pos + 1
            } else {
                content_start
            };
            let start = snapshot.anchor_before(MultiBufferOffset(display_start));
            let end = snapshot.anchor_after(MultiBufferOffset(content_end));

            // Check if this wikilink has a diagnostic (unresolved link)
            let has_diagnostic = all_diagnostics.iter().any(|(diag_range, _msg)| {
                diag_range.start < wikilink.range.end && diag_range.end > wikilink.range.start
            });

            if has_diagnostic {
                unresolved_wikilink_ranges.push(start..end);
            } else {
                wikilink_ranges.push(start..end);
            }
        }
    }

    set_or_clear_highlight(
        editor,
        HighlightKey::MarkdownWysiwygWikilink,
        wikilink_ranges,
        HighlightStyle {
            color: Some(Hsla {
                h: 0.6,
                s: 0.5,
                l: 0.6,
                a: 1.0,
            }),
            background_color: Some(Hsla {
                h: 0.6,
                s: 0.2,
                l: 0.3,
                a: 0.15,
            }),
            font_weight: Some(FontWeight::BOLD),
            font_style: Some(gpui::FontStyle::Normal),
            underline: Some(gpui::UnderlineStyle {
                thickness: gpui::px(1.0),
                color: Some(Hsla {
                    h: 0.6,
                    s: 0.5,
                    l: 0.6,
                    a: 0.6,
                }),
                wavy: false,
            }),
            ..Default::default()
        },
        cx,
    );

    // Unresolved wikilinks get a pastel blue color
    set_or_clear_highlight(
        editor,
        HighlightKey::MarkdownWysiwygUnresolvedLink,
        unresolved_wikilink_ranges,
        HighlightStyle {
            color: Some(Hsla {
                h: 0.58,
                s: 0.4,
                l: 0.65,
                a: 0.7,
            }),
            background_color: Some(Hsla {
                h: 0.58,
                s: 0.2,
                l: 0.3,
                a: 0.1,
            }),
            font_weight: Some(FontWeight::BOLD),
            font_style: Some(gpui::FontStyle::Normal),
            underline: Some(gpui::UnderlineStyle {
                thickness: gpui::px(1.0),
                color: Some(Hsla {
                    h: 0.58,
                    s: 0.4,
                    l: 0.65,
                    a: 0.5,
                }),
                wavy: true,
            }),
            ..Default::default()
        },
        cx,
    );

    let background = cx.theme().colors().editor_background;
    set_or_clear_highlight(
        editor,
        HighlightKey::MarkdownWysiwygMarker,
        marker_ranges,
        HighlightStyle {
            color: Some(background),
            background_color: Some(background),
            ..Default::default()
        },
        cx,
    );
}

fn set_or_clear_highlight(
    editor: &mut Editor,
    key: HighlightKey,
    ranges: Vec<Range<multi_buffer::Anchor>>,
    style: HighlightStyle,
    cx: &mut Context<Editor>,
) {
    if ranges.is_empty() {
        editor.clear_highlights(key, cx);
    } else {
        editor.highlight_text(key, ranges, style, cx);
    }
}

fn collect_block_replaced_ranges(
    decorations: &MarkdownDecorations,
    active_range: &Range<usize>,
) -> Vec<Range<usize>> {
    let mut block_ranges: Vec<Range<usize>> = Vec::new();
    for heading in &decorations.headings {
        if !range_on_cursor_line(&heading.line_range, active_range) {
            block_ranges.push(heading.line_range.clone());
        }
    }
    for table in &decorations.tables {
        if !range_on_cursor_line(&table.range, active_range) {
            block_ranges.push(table.range.clone());
        }
    }
    for image in &decorations.images {
        if !range_on_cursor_line(&image.range, active_range) {
            block_ranges.push(image.range.clone());
        }
    }
    for rule in &decorations.horizontal_rules {
        if !range_on_cursor_line(&rule.range, active_range) {
            block_ranges.push(rule.range.clone());
        }
    }
    block_ranges.sort_by_key(|range| range.start);
    block_ranges
}

fn callout_appearance(kind: &str) -> (&'static str, Hsla) {
    match kind {
        "note" | "info" => (
            "ⓘ",
            Hsla {
                h: 0.58,
                s: 0.6,
                l: 0.55,
                a: 1.0,
            },
        ),
        "tip" | "hint" | "important" => (
            "⚑",
            Hsla {
                h: 0.45,
                s: 0.6,
                l: 0.45,
                a: 1.0,
            },
        ),
        "warning" | "caution" | "attention" => (
            "⚠",
            Hsla {
                h: 0.1,
                s: 0.8,
                l: 0.5,
                a: 1.0,
            },
        ),
        "danger" | "error" | "bug" | "failure" | "fail" => (
            "✖",
            Hsla {
                h: 0.0,
                s: 0.7,
                l: 0.55,
                a: 1.0,
            },
        ),
        "question" | "help" | "faq" => (
            "?",
            Hsla {
                h: 0.12,
                s: 0.7,
                l: 0.5,
                a: 1.0,
            },
        ),
        "success" | "check" | "done" => (
            "✓",
            Hsla {
                h: 0.35,
                s: 0.6,
                l: 0.45,
                a: 1.0,
            },
        ),
        "quote" | "cite" => (
            "❝",
            Hsla {
                h: 0.0,
                s: 0.0,
                l: 0.55,
                a: 1.0,
            },
        ),
        "example" => (
            "☰",
            Hsla {
                h: 0.75,
                s: 0.4,
                l: 0.6,
                a: 1.0,
            },
        ),
        _ => (
            "ⓘ",
            Hsla {
                h: 0.58,
                s: 0.6,
                l: 0.55,
                a: 1.0,
            },
        ),
    }
}

fn marker_overlaps_block(marker: &Range<usize>, block_ranges: &[Range<usize>]) -> bool {
    block_ranges.iter().any(|block| {
        marker.start < block.end && marker.end > block.start
    })
}

fn apply_marker_folds(
    editor: &mut Editor,
    _snapshot: &MultiBufferSnapshot,
    decorations: &MarkdownDecorations,
    _cursor: usize,
    active_range: &Range<usize>,
    restrict_to: Option<&Range<usize>>,
    cx: &mut Context<Editor>,
) {
    // When restricted, only fold markers that fall on the given line range. This
    // lets cursor moves re-fold just the line the cursor left rather than every
    // marker in the document.
    let within_restriction = |range: &Range<usize>| {
        restrict_to.is_none_or(|restrict| range_on_cursor_line(range, restrict))
    };
    let block_ranges = collect_block_replaced_ranges(decorations, active_range);
    let placeholder = FoldPlaceholder {
        render: Arc::new(|_fold_id, _range, _cx| gpui::Empty.into_any_element()),
        constrain_width: false,
        merge_adjacent: false,
        type_tag: Some(TypeId::of::<WysiwygFoldTag>()),
        collapsed_text: Some("".into()),
    };

    let bullet_color = Hsla {
        h: 0.6,
        s: 0.2,
        l: 0.6,
        a: 1.0,
    };
    let bullet_placeholder = FoldPlaceholder {
        render: Arc::new(move |_fold_id, _range, _cx| {
            gpui::div()
                .flex()
                .items_center()
                .text_color(bullet_color)
                .child(SharedString::from("• "))
                .into_any_element()
        }),
        constrain_width: false,
        merge_adjacent: false,
        type_tag: Some(TypeId::of::<WysiwygFoldTag>()),
        collapsed_text: Some("• ".into()),
    };

    let mut creases = Vec::new();
    for marker in &decorations.syntax_markers {
        if marker.range.start < marker.range.end
            && within_restriction(&marker.range)
            && !marker_overlaps_block(&marker.range, &block_ranges)
            && !range_on_cursor_line(&marker.range, active_range)
        {
            let start = MultiBufferOffset(marker.range.start);
            let end = MultiBufferOffset(marker.range.end);
            creases.push(Crease::simple(start..end, placeholder.clone()));
        }
    }

    for list_item in &decorations.list_items {
        if within_restriction(&list_item.marker_range)
            && !marker_overlaps_block(&list_item.marker_range, &block_ranges)
            && !range_on_cursor_line(&list_item.marker_range, active_range)
        {
            let start = MultiBufferOffset(list_item.marker_range.start);
            let end = MultiBufferOffset(list_item.marker_range.end);
            creases.push(Crease::simple(start..end, bullet_placeholder.clone()));
        }
    }

    for task in &decorations.task_checkboxes {
        if within_restriction(&task.marker_range)
            && !marker_overlaps_block(&task.marker_range, &block_ranges)
            && !range_on_cursor_line(&task.marker_range, active_range)
        {
            let glyph = if task.checked { "☑" } else { "☐" };
            let color = if task.checked {
                Hsla {
                    h: 0.35,
                    s: 0.5,
                    l: 0.5,
                    a: 1.0,
                }
            } else {
                Hsla {
                    h: 0.0,
                    s: 0.0,
                    l: 0.55,
                    a: 1.0,
                }
            };
            let placeholder = FoldPlaceholder {
                render: Arc::new(move |_fold_id, _range, _cx| {
                    gpui::div()
                        .flex()
                        .items_center()
                        .text_color(color)
                        .child(SharedString::from(glyph))
                        .into_any_element()
                }),
                constrain_width: true,
                merge_adjacent: false,
                type_tag: Some(TypeId::of::<WysiwygFoldTag>()),
                collapsed_text: Some(glyph.into()),
            };
            let start = MultiBufferOffset(task.marker_range.start);
            let end = MultiBufferOffset(task.marker_range.end);
            creases.push(Crease::simple(start..end, placeholder));
        }
    }

    let quote_bar_color = Hsla {
        h: 0.0,
        s: 0.0,
        l: 0.5,
        a: 0.45,
    };
    // Draw the left accent as a full-line-height filled bar rather than a glyph
    // so that consecutive quote lines join into one continuous rule, matching
    // Obsidian. The bar is a flat (non-rounded) rule and the trailing gap
    // separates it from the quoted text.
    let quote_placeholder = FoldPlaceholder {
        render: Arc::new(move |_fold_id, _range, _cx| {
            gpui::div()
                .flex()
                .items_center()
                .h_full()
                .pr(gpui::px(14.0))
                .child(
                    gpui::div()
                        .w(gpui::px(3.0))
                        .h_full()
                        .bg(quote_bar_color),
                )
                .into_any_element()
        }),
        constrain_width: false,
        merge_adjacent: false,
        type_tag: Some(TypeId::of::<WysiwygFoldTag>()),
        collapsed_text: Some("▍ ".into()),
    };

    for blockquote in &decorations.blockquotes {
        if within_restriction(&blockquote.marker_range)
            && !marker_overlaps_block(&blockquote.marker_range, &block_ranges)
            && !range_on_cursor_line(&blockquote.marker_range, active_range)
        {
            let start = MultiBufferOffset(blockquote.marker_range.start);
            let end = MultiBufferOffset(blockquote.marker_range.end);
            creases.push(Crease::simple(start..end, quote_placeholder.clone()));
        }
    }

    for callout in &decorations.callouts {
        if within_restriction(&callout.marker_range)
            && !marker_overlaps_block(&callout.marker_range, &block_ranges)
            && !range_on_cursor_line(&callout.marker_range, active_range)
        {
            let (icon, color) = callout_appearance(&callout.kind);
            let mut title: String = callout.kind.chars().collect();
            if let Some(first) = title.get_mut(0..1) {
                first.make_ascii_uppercase();
            }
            let label = SharedString::from(format!("▍ {} {} ", icon, title));
            let placeholder = FoldPlaceholder {
                render: Arc::new({
                    let label = label.clone();
                    move |_fold_id, _range, _cx| {
                        gpui::div()
                            .flex()
                            .items_center()
                            .font_family(wysiwyg_serif_family())
                            .text_color(color)
                            .font_weight(FontWeight::BOLD)
                            .child(label.clone())
                            .into_any_element()
                    }
                }),
                constrain_width: true,
                merge_adjacent: false,
                type_tag: Some(TypeId::of::<WysiwygFoldTag>()),
                collapsed_text: Some(label),
            };
            let start = MultiBufferOffset(callout.marker_range.start);
            let end = MultiBufferOffset(callout.marker_range.end);
            creases.push(Crease::simple(start..end, placeholder));
        }
    }

    for block_id in &decorations.block_ids {
        if within_restriction(&block_id.range)
            && !marker_overlaps_block(&block_id.range, &block_ranges)
            && !range_on_cursor_line(&block_id.range, active_range)
        {
            let start = MultiBufferOffset(block_id.range.start);
            let end = MultiBufferOffset(block_id.range.end);
            creases.push(Crease::simple(start..end, placeholder.clone()));
        }
    }

    if !creases.is_empty() {
        editor.display_map.update(cx, |map, cx| map.fold(creases, cx));
        cx.notify();
    }
}

fn remove_stale_folds(editor: &mut Editor, cx: &mut Context<Editor>) {
    let type_id = TypeId::of::<WysiwygFoldTag>();
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let buffer_len = snapshot.len();
    let full_range = vec![MultiBufferOffset(0)..buffer_len];
    editor.remove_folds_with_type(&full_range, type_id, false, cx);
}

fn apply_blocks(
    editor: &mut Editor,
    snapshot: &MultiBufferSnapshot,
    decorations: &MarkdownDecorations,
    active_range: &Range<usize>,
    cx: &mut Context<Editor>,
) {
    // Collect old blocks with their anchored ranges for smart diffing.
    let old_blocks: Vec<(CustomBlockId, Range<Anchor>)> =
        editor.markdown_wysiwyg_state.block_ids.drain(..).collect();

    // Build desired block properties and track both the source offset range
    // (for diffing against the freshly parsed decorations) and the anchored
    // range (for storage, so the block tracks later edits without churn).
    let mut block_properties: Vec<BlockProperties<Anchor>> = Vec::new();
    let mut block_ranges: Vec<Range<usize>> = Vec::new();
    let mut block_anchors: Vec<Range<Anchor>> = Vec::new();

    for heading in &decorations.headings {
        if range_on_cursor_line(&heading.line_range, active_range) {
            continue;
        }

        let start = snapshot.anchor_before(MultiBufferOffset(heading.line_range.start));
        let end = snapshot.anchor_after(MultiBufferOffset(heading.line_range.end));

        let level = heading.level;
        let buffer_text = snapshot.text();
        let display_text: String = buffer_text
            .get(heading.line_range.clone())
            .unwrap_or_default()
            .trim_start_matches('#')
            .trim_start()
            .to_string();

        let render: RenderBlock = Arc::new(move |block_context: &mut BlockContext| {
            let clamped_level = level.min(4);
            let font_size_multiplier = match clamped_level {
                1 => 2.6_f32,
                2 => 2.08,
                3 => 1.69,
                _ => 1.43,
            };
            let base_size = block_context.em_width;
            let scaled_size = base_size * font_size_multiplier;
            let left_margin = block_context.anchor_x;

            gpui::div()
                .pl(left_margin)
                .font_family(wysiwyg_serif_family())
                .text_size(scaled_size)
                .font_weight(FontWeight::BOLD)
                .line_height(scaled_size * 1.0)
                .child(SharedString::from(display_text.clone()))
                .into_any_element()
        });

        block_ranges.push(heading.line_range.clone());
        block_anchors.push(start..end);
        block_properties.push(BlockProperties {
            placement: BlockPlacement::Replace(start..=end),
            height: Some(1),
            style: BlockStyle::Flex,
            render,
            priority: 0,
        });
    }

    for (table_index, table) in decorations.tables.iter().enumerate() {
        if range_on_cursor_line(&table.range, active_range) {
            continue;
        }

        let start = snapshot.anchor_before(MultiBufferOffset(table.range.start));
        let end = snapshot.anchor_after(MultiBufferOffset(table.range.end));

        let headers = table.headers.clone();
        let rows = table.rows.clone();
        let row_count = rows.len() as u32 + 1;

        let max_col_width_px: f32 = 50.0;
        let padding_px: f32 = 20.0;

        let column_count = headers.len();
        let mut column_widths: Vec<f32> = Vec::with_capacity(column_count);
        for (col_index, header) in headers.iter().enumerate() {
            let mut max_len = header.len();
            for row in &rows {
                if let Some(cell) = row.get(col_index) {
                    max_len = max_len.max(cell.len());
                }
            }
            let text_based_width = (max_len as f32) * 7.0 + padding_px;
            let clamped_width = text_based_width.min(max_col_width_px * 7.0);
            column_widths.push(clamped_width);
        }

        let render: RenderBlock = Arc::new(move |block_context: &mut BlockContext| {
            let border_color = Hsla {
                h: 0.0,
                s: 0.0,
                l: 0.5,
                a: 0.3,
            };
            let left_margin = block_context.anchor_x;

            let mut inner_table = gpui::div()
                .flex()
                .flex_col()
                .font_family(wysiwyg_serif_family())
                .border_1()
                .border_color(border_color);

            let mut header_row = gpui::div()
                .flex()
                .flex_row()
                .flex_shrink_0()
                .border_b_1()
                .border_color(border_color)
                .bg(Hsla {
                    h: 0.0,
                    s: 0.0,
                    l: 0.3,
                    a: 0.1,
                });

            for (col_index, header) in headers.iter().enumerate() {
                let col_width = column_widths.get(col_index).copied().unwrap_or(100.0);
                header_row = header_row.child(
                    gpui::div()
                        .w(gpui::px(col_width))
                        .flex_shrink_0()
                        .px_2()
                        .py_1()
                        .font_weight(FontWeight::BOLD)
                        .border_r_1()
                        .border_color(border_color)
                        .child(SharedString::from(header.clone())),
                );
            }
            inner_table = inner_table.child(header_row);

            for row in &rows {
                let mut row_element = gpui::div()
                    .flex()
                    .flex_row()
                    .flex_shrink_0()
                    .border_b_1()
                    .border_color(border_color);

                for (col_index, cell) in row.iter().enumerate() {
                    let col_width = column_widths.get(col_index).copied().unwrap_or(100.0);
                    row_element = row_element.child(
                        gpui::div()
                            .w(gpui::px(col_width))
                            .flex_shrink_0()
                            .px_2()
                            .py_1()
                            .border_r_1()
                            .border_color(border_color)
                            .child(SharedString::from(cell.clone())),
                    );
                }
                inner_table = inner_table.child(row_element);
            }

            let table_total_width: f32 = column_widths.iter().sum();
            let scrollable_wrapper = gpui::div()
                .id(ElementId::Name(
                    SharedString::from(format!("wysiwyg-table-{}", table_index)),
                ))
                .overflow_x_scroll()
                .max_w(block_context.max_width * 0.7)
                .child(inner_table.w(gpui::px(table_total_width)));

            gpui::div()
                .pl(left_margin)
                .child(scrollable_wrapper)
                .into_any_element()
        });

        block_ranges.push(table.range.clone());
        block_anchors.push(start..end);
        block_properties.push(BlockProperties {
            placement: BlockPlacement::Replace(start..=end),
            height: Some(row_count + 1),
            style: BlockStyle::Flex,
            render,
            priority: 0,
        });
    }

    let document_dir = document_directory(editor, cx);
    for image in &decorations.images {
        if range_on_cursor_line(&image.range, active_range) {
            continue;
        }

        let start = snapshot.anchor_before(MultiBufferOffset(image.range.start));
        let end = snapshot.anchor_after(MultiBufferOffset(image.range.end));

        let url = image.url.clone();
        let is_local = !url.starts_with("http://") && !url.starts_with("https://");
        let resolved_path = if is_local {
            let raw_path = PathBuf::from(&url);
            // Relative references resolve against the document's own directory
            // (Obsidian behavior), not the process working directory.
            let candidate = if raw_path.is_absolute() {
                raw_path
            } else if let Some(dir) = &document_dir {
                dir.join(&raw_path)
            } else {
                raw_path
            };
            if candidate.exists() {
                Some(candidate)
            } else {
                None
            }
        } else {
            None
        };
        let image_width = image.width;
        let image_height = image.height;

        let (render, height): (RenderBlock, u32) = if is_video_url(&url) {
            let element_id: ElementId =
                SharedString::from(format!("wysiwyg-video-embed-{}", image.range.start)).into();
            let label = video_label(&url);
            let render: RenderBlock = Arc::new(move |block_context: &mut BlockContext| {
                let left_margin = block_context.anchor_x;
                let colors = block_context.app.theme().colors();
                let border_color = colors.border;
                let background = colors.element_background;
                let icon_color = colors.text_muted;
                let label_color = colors.text;
                let target_path = resolved_path.clone();
                let target_url = url.clone();

                gpui::div()
                    .pl(left_margin)
                    .py_1()
                    .child(
                        gpui::div()
                            .id(element_id.clone())
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_3()
                            .py_2()
                            .max_w(gpui::px(360.0))
                            .rounded_md()
                            .border_1()
                            .border_color(border_color)
                            .bg(background)
                            .cursor_pointer()
                            .child(gpui::div().text_color(icon_color).child(SharedString::from("▶")))
                            .child(gpui::div().text_color(label_color).child(label.clone()))
                            .on_click(move |_event, _window, cx| {
                                if let Some(path) = &target_path {
                                    cx.open_with_system(path);
                                } else {
                                    cx.open_url(&target_url);
                                }
                            }),
                    )
                    .into_any_element()
            });
            (render, 3)
        } else {
            let render: RenderBlock = Arc::new(move |block_context: &mut BlockContext| {
                let left_margin = block_context.anchor_x;
                let mut image_element = if let Some(path) = &resolved_path {
                    gpui::img(path.clone())
                } else {
                    gpui::img(SharedString::from(url.clone()))
                };

                if let Some(width) = image_width {
                    image_element = image_element.w(gpui::px(width as f32));
                }
                if let Some(height) = image_height {
                    image_element = image_element.h(gpui::px(height as f32));
                }
                if image_width.is_none() && image_height.is_none() {
                    image_element = image_element.max_w(gpui::px(300.0));
                }

                gpui::div()
                    .pl(left_margin)
                    .py_1()
                    .child(image_element)
                    .into_any_element()
            });
            (render, 10)
        };

        block_ranges.push(image.range.clone());
        block_anchors.push(start..end);
        block_properties.push(BlockProperties {
            placement: BlockPlacement::Replace(start..=end),
            height: Some(height),
            style: BlockStyle::Flex,
            render,
            priority: 0,
        });
    }

    for rule in &decorations.horizontal_rules {
        if range_on_cursor_line(&rule.range, active_range) {
            continue;
        }

        let start = snapshot.anchor_before(MultiBufferOffset(rule.range.start));
        let end = snapshot.anchor_after(MultiBufferOffset(rule.range.end));

        let render: RenderBlock = Arc::new(move |block_context: &mut BlockContext| {
            let left_margin = block_context.anchor_x;
            let rule_width =
                (block_context.em_width * READABLE_LINE_LENGTH as f32).min(block_context.max_width);
            gpui::div()
                .pl(left_margin)
                .flex()
                .items_center()
                .h_full()
                .child(
                    gpui::div()
                        .h(gpui::px(2.0))
                        .w(rule_width)
                        .rounded_full()
                        .bg(Hsla {
                            h: 0.0,
                            s: 0.0,
                            l: 0.5,
                            a: 0.4,
                        }),
                )
                .into_any_element()
        });

        block_ranges.push(rule.range.clone());
        block_anchors.push(start..end);
        block_properties.push(BlockProperties {
            placement: BlockPlacement::Replace(start..=end),
            height: Some(1),
            style: BlockStyle::Flex,
            render,
            priority: 0,
        });
    }

    // Smart diff: an existing block's stored anchors resolve to its current
    // offset range, so a block whose content didn't change still matches the
    // freshly parsed range even after edits shifted it. Such blocks are kept in
    // place; only blocks that genuinely appeared or disappeared are inserted or
    // removed, which keeps the layout below the cursor stable while typing.
    let mut kept_blocks: Vec<(CustomBlockId, Range<Anchor>)> = Vec::new();
    let mut to_remove: HashSet<CustomBlockId> = HashSet::default();
    let mut new_matched: Vec<bool> = vec![false; block_ranges.len()];

    for (id, old_anchor_range) in old_blocks {
        let resolved =
            old_anchor_range.start.to_offset(snapshot).0..old_anchor_range.end.to_offset(snapshot).0;
        let mut matched = false;
        for (i, new_range) in block_ranges.iter().enumerate() {
            if !new_matched[i] && resolved == *new_range {
                kept_blocks.push((id, old_anchor_range));
                new_matched[i] = true;
                matched = true;
                break;
            }
        }
        if !matched {
            to_remove.insert(id);
        }
    }

    if !to_remove.is_empty() {
        editor.remove_blocks(to_remove, None, cx);
    }

    // Only insert blocks for ranges that weren't already present.
    let new_properties: Vec<BlockProperties<Anchor>> = block_properties
        .into_iter()
        .enumerate()
        .filter(|(i, _)| !new_matched[*i])
        .map(|(_, p)| p)
        .collect();
    let new_block_anchors: Vec<Range<Anchor>> = block_anchors
        .into_iter()
        .enumerate()
        .filter(|(i, _)| !new_matched[*i])
        .map(|(_, r)| r)
        .collect();

    if !new_properties.is_empty() {
        let new_ids = editor.insert_blocks(new_properties, None, cx);
        for (id, anchor_range) in new_ids.into_iter().zip(new_block_anchors) {
            kept_blocks.push((id, anchor_range));
        }
    }

    editor.markdown_wysiwyg_state.block_ids = kept_blocks;

    // Render references section if we have cached references
    apply_references_block(editor, snapshot, cx);
}

fn apply_references_block(
    editor: &mut Editor,
    snapshot: &MultiBufferSnapshot,
    cx: &mut Context<Editor>,
) {
    let references = editor.markdown_wysiwyg_state.cached_references.clone();

    // The block's anchor follows the document end across edits, so it only needs
    // rebuilding when the reference list itself changes. Skipping when unchanged
    // avoids removing and re-inserting the block on every cursor move.
    let already_rendered = !editor.markdown_wysiwyg_state.references_block_ids.is_empty()
        && editor.markdown_wysiwyg_state.rendered_references == references;
    if already_rendered {
        return;
    }

    let old_ref_ids: HashSet<CustomBlockId> = editor
        .markdown_wysiwyg_state
        .references_block_ids
        .drain(..)
        .collect();
    if !old_ref_ids.is_empty() {
        editor.remove_blocks(old_ref_ids, None, cx);
    }

    if references.is_empty() {
        editor.markdown_wysiwyg_state.rendered_references.clear();
        return;
    }

    editor.markdown_wysiwyg_state.rendered_references = references.clone();

    // Place the references block after the last line of the document
    let buffer_end = snapshot.len();
    let end_anchor = snapshot.anchor_after(buffer_end);

    let ref_count = references.len();
    let height = (ref_count as u32) + 2; // +2 for header and divider

    let render: RenderBlock = Arc::new(move |block_context: &mut BlockContext| {
        let left_margin = block_context.anchor_x;
        let border_color = Hsla {
            h: 0.0,
            s: 0.0,
            l: 0.5,
            a: 0.2,
        };

        let mut container = gpui::div()
            .pl(left_margin)
            .pt_4()
            .flex()
            .flex_col()
            .gap_1();

        // Divider line
        container = container.child(
            gpui::div()
                .h(gpui::px(1.0))
                .bg(border_color)
                .mb_2(),
        );

        // Header
        container = container.child(
            gpui::div()
                .text_size(block_context.em_width * 1.1)
                .font_weight(FontWeight::BOLD)
                .text_color(Hsla {
                    h: 0.0,
                    s: 0.0,
                    l: 0.6,
                    a: 1.0,
                })
                .mb_1()
                .child(SharedString::from("Linked Mentions")),
        );

        // Reference entries
        for ref_name in &references {
            container = container.child(
                gpui::div()
                    .flex()
                    .flex_row()
                    .gap_1()
                    .child(
                        gpui::div()
                            .text_color(Hsla {
                                h: 0.6,
                                s: 0.5,
                                l: 0.6,
                                a: 1.0,
                            })
                            .child(SharedString::from(format!("  {}", ref_name))),
                    ),
            );
        }

        container.into_any_element()
    });

    let block_properties = vec![BlockProperties {
        placement: BlockPlacement::Below(end_anchor),
        height: Some(height),
        style: BlockStyle::Flex,
        render,
        priority: 0,
    }];

    let new_ids = editor.insert_blocks(block_properties, None, cx);
    editor.markdown_wysiwyg_state.references_block_ids = new_ids;
}

/// Fetches references for the current document from the LSP and caches them.
/// This is called when WYSIWYG mode is toggled on or when the document changes.
pub fn fetch_references(editor: &mut Editor, cx: &mut Context<Editor>) {
    if !editor.markdown_wysiwyg_state.active {
        return;
    }

    // Get the current file's absolute path and stem name
    let multi_buffer = editor.buffer().read(cx);
    let buffers = multi_buffer.all_buffers();
    let mut current_file_stem: Option<String> = None;
    let mut workspace_dir: Option<PathBuf> = None;

    for buffer in buffers {
        let buffer_read = buffer.read(cx);
        if let Some(file) = buffer_read.file() {
            current_file_stem = file.path().file_stem().map(|s| s.to_string());
            if let Some(local_file) = file.as_local() {
                let abs = local_file.abs_path(cx);
                workspace_dir = abs.parent().map(|p| p.to_path_buf());
            }
            break;
        }
    }

    let file_stem = match current_file_stem {
        Some(s) => s,
        None => return,
    };
    let dir = match workspace_dir {
        Some(d) => d,
        None => return,
    };

    // Scan all .md files in the workspace directory for wikilinks to this file
    editor.markdown_wysiwyg_state.references_task = cx.spawn(async move |editor, cx| {
        let mut backlink_files: Vec<String> = Vec::new();

        // Read all .md files in the directory
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("md") {
                    continue;
                }
                // Skip the current file
                let entry_stem = path.file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string();
                if entry_stem == file_stem {
                    continue;
                }

                // Read the file and check for wikilinks to the current file
                if let Ok(content) = std::fs::read_to_string(&path) {
                    // Look for [[file_stem]] or [[file_stem|display]] or [[file_stem#section]]
                    let pattern_simple = format!("[[{}]]", file_stem);
                    let pattern_pipe = format!("[[{}|", file_stem);
                    let pattern_hash = format!("[[{}#", file_stem);

                    if content.contains(&pattern_simple)
                        || content.contains(&pattern_pipe)
                        || content.contains(&pattern_hash)
                    {
                        backlink_files.push(entry_stem);
                    }
                }
            }
        }

        backlink_files.sort();

        editor.update(cx, |editor, cx| {
            editor.markdown_wysiwyg_state.cached_references = backlink_files;
            if editor.markdown_wysiwyg_state.active {
                refresh_wysiwyg_decorations(editor, cx);
            }
        }).ok();
    });
}

fn clear_wysiwyg_decorations(editor: &mut Editor, cx: &mut Context<Editor>) {
    editor.clear_highlights(HighlightKey::MarkdownWysiwygBold, cx);
    editor.clear_highlights(HighlightKey::MarkdownWysiwygItalic, cx);
    editor.clear_highlights(HighlightKey::MarkdownWysiwygStrikethrough, cx);
    editor.clear_highlights(HighlightKey::MarkdownWysiwygCode, cx);
    editor.clear_highlights(HighlightKey::MarkdownWysiwygHeading, cx);
    editor.clear_highlights(HighlightKey::MarkdownWysiwygMarker, cx);
    editor.clear_highlights(HighlightKey::MarkdownWysiwygWikilink, cx);
    editor.clear_highlights(HighlightKey::MarkdownWysiwygUnresolvedLink, cx);
    editor.clear_highlights(HighlightKey::MarkdownWysiwygHighlight, cx);
    editor.clear_highlights(HighlightKey::MarkdownWysiwygExternalLink, cx);
    editor.clear_highlights(HighlightKey::MarkdownWysiwygListMarker, cx);

    let type_id = TypeId::of::<WysiwygFoldTag>();
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let buffer_len = snapshot.len();
    let full_range = vec![
        MultiBufferOffset(0)..buffer_len,
    ];
    editor.remove_folds_with_type(&full_range, type_id, false, cx);

    let old_block_ids: HashSet<CustomBlockId> = editor
        .markdown_wysiwyg_state
        .block_ids
        .drain(..)
        .map(|(id, _)| id)
        .collect();
    if !old_block_ids.is_empty() {
        editor.remove_blocks(old_block_ids, None, cx);
    }

    let old_ref_ids: HashSet<CustomBlockId> = editor
        .markdown_wysiwyg_state
        .references_block_ids
        .drain(..)
        .collect();
    if !old_ref_ids.is_empty() {
        editor.remove_blocks(old_ref_ids, None, cx);
    }
    editor.markdown_wysiwyg_state.cached_references.clear();
    editor.markdown_wysiwyg_state.rendered_references.clear();
    editor.markdown_wysiwyg_state.previous_active_line_range = None;
    editor.markdown_wysiwyg_state.last_structure_fingerprint = None;
}

pub fn on_selection_changed(editor: &mut Editor, window: &mut Window, cx: &mut Context<Editor>) {
    if !editor.markdown_wysiwyg_state.active || editor.markdown_wysiwyg_state.adjusting_cursor {
        return;
    }

    let snapshot = editor.buffer().read(cx).snapshot(cx);

    // Skip the full reparse when the selection still covers the same buffer
    // rows. Typing extends the active line's offsets but never changes which
    // rows are revealed, so this avoids re-parsing the document on every
    // keystroke (the dominant source of typing lag).
    let anchor = editor.selections.newest_anchor();
    let head_row = snapshot.offset_to_point(anchor.head().to_offset(&snapshot)).row;
    let tail_row = snapshot.offset_to_point(anchor.tail().to_offset(&snapshot)).row;
    let active_rows = head_row.min(tail_row)..head_row.max(tail_row);
    if editor.markdown_wysiwyg_state.previous_active_rows.as_ref() == Some(&active_rows) {
        return;
    }
    editor.markdown_wysiwyg_state.previous_active_rows = Some(active_rows);

    let text = snapshot.text();
    let cursor = cursor_offset(editor, cx);

    let heading_adjustment = {
        let decorations = parse_markdown_decorations(&text);
        let mut adjustment: Option<usize> = None;
        for heading in &decorations.headings {
            if cursor == heading.line_range.start {
                let was_block = editor.markdown_wysiwyg_state.block_ids
                    .iter()
                    .any(|(_, anchor_range)| {
                        anchor_range.start.to_offset(&snapshot).0 == heading.line_range.start
                    });
                if was_block {
                    let prefix_len = heading.level as usize;
                    let space_after = if text.as_bytes().get(heading.line_range.start + prefix_len) == Some(&b' ') {
                        1
                    } else {
                        0
                    };
                    let content_start = heading.line_range.start + prefix_len + space_after;
                    if content_start <= heading.line_range.end {
                        adjustment = Some(content_start);
                    }
                }
                break;
            }
        }
        adjustment
    };

    // Refresh decorations before adjusting the cursor. This removes the heading
    // Replace block so that anchor_before resolves to the correct offset inside
    // the (now un-replaced) heading text.
    refresh_active_line_decorations(editor, cx);

    if let Some(target_offset) = heading_adjustment {
        editor.markdown_wysiwyg_state.adjusting_cursor = true;
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let anchor = snapshot.anchor_before(MultiBufferOffset(target_offset));
        use crate::SelectionEffects;
        editor.change_selections(SelectionEffects::default(), window, cx, |s| {
            s.select_anchor_ranges([anchor..anchor]);
        });
        editor.markdown_wysiwyg_state.adjusting_cursor = false;
    }
}

/// Checks if the cursor is on a wikilink in WYSIWYG mode, and if so,
/// triggers go-to-definition. Returns true if a wikilink click was handled.
#[allow(dead_code)]
pub fn try_handle_wikilink_click(
    editor: &mut Editor,
    window: &mut Window,
    cx: &mut Context<Editor>,
) -> bool {
    if !editor.markdown_wysiwyg_state.active {
        return false;
    }

    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let text = snapshot.text();
    let cursor = cursor_offset(editor, cx);

    // Check if cursor is inside a wikilink
    let wikilinks = parse_wikilinks(&text);
    for wikilink in &wikilinks {
        if cursor >= wikilink.range.start && cursor <= wikilink.range.end {
            // Cursor is inside a wikilink — trigger go-to-definition
            // Position cursor at the link content (inside [[ ]])
            let content_start = wikilink.range.start + 2;
            let link_end = wikilink.range.end - 2;
            let inner = &text[content_start..link_end];
            // If there's a pipe, position at the link part (before pipe)
            let link_target_end = if let Some(pipe_pos) = inner.find('|') {
                content_start + pipe_pos
            } else {
                link_end
            };
            // Move cursor to middle of the link target for go-to-definition
            let target_pos = (content_start + link_target_end) / 2;
            let anchor = snapshot.anchor_before(MultiBufferOffset(target_pos));

            // Set the cursor position to be inside the wikilink content
            use crate::SelectionEffects;
            editor.change_selections(SelectionEffects::default(), window, cx, |selections| {
                selections.select_anchor_ranges([anchor..anchor]);
            });

            // Now trigger go-to-definition
            use crate::GoToDefinition;
            let _ = editor.go_to_definition(&GoToDefinition, window, cx);
            return true;
        }
    }

    false
}

/// Attempts to handle an image paste when WYSIWYG mode is active.
/// Returns true if an image was handled, false otherwise (so normal paste continues).
pub fn try_handle_image_paste(
    editor: &mut Editor,
    image: &gpui::Image,
    window: &mut Window,
    cx: &mut Context<Editor>,
) -> bool {
    if !editor.markdown_wysiwyg_state.active {
        return false;
    }

    let extension = match image.format {
        gpui::ImageFormat::Png => "png",
        gpui::ImageFormat::Jpeg => "jpg",
        gpui::ImageFormat::Webp => "webp",
        gpui::ImageFormat::Gif => "gif",
        _ => "png",
    };

    let save_dir = match document_directory(editor, cx) {
        Some(dir) => dir,
        None => return false,
    };

    // Generate a unique filename using a timestamp
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let filename = format!("pasted-image-{}.{}", timestamp, extension);
    let save_path = save_dir.join(&filename);

    // Write the image bytes to disk
    if std::fs::write(&save_path, &image.bytes).is_err() {
        return false;
    }

    // Insert a wikilink image reference at the cursor
    let wikilink_text = format!("![[{}]]", filename);
    editor.insert(&wikilink_text, window, cx);

    // Trigger a WYSIWYG refresh so the image renders immediately
    schedule_wysiwyg_refresh(editor, cx);

    true
}

/// Returns the directory containing the editor's document, used as the base
/// for resolving relative image references and for storing pasted/dropped
/// images alongside the markdown file (Obsidian-vault behavior).
fn document_directory(editor: &Editor, cx: &App) -> Option<PathBuf> {
    let multi_buffer = editor.buffer().read(cx);
    for buffer in multi_buffer.all_buffers() {
        let buffer_read = buffer.read(cx);
        if let Some(file) = buffer_read.file()
            && let Some(local_file) = file.as_local()
        {
            return local_file.abs_path(cx).parent().map(Path::to_path_buf);
        }
    }
    None
}

/// Whether `path` looks like an image we can render inline.
fn is_image_path(path: &Path) -> bool {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase);
    matches!(
        extension.as_deref(),
        Some(
            "png" | "jpg"
                | "jpeg"
                | "gif"
                | "webp"
                | "bmp"
                | "svg"
                | "avif"
                | "ico"
                | "tiff"
                | "tif"
        )
    )
}

/// Whether an embed reference points at a video file. GPUI has no general
/// video-playback element (its `Surface` is a macOS-only CoreVideo buffer used
/// for screen sharing, not a file player), so video embeds are rendered as a
/// clickable card rather than an inline `gpui::img`, which would fail to decode.
fn is_video_url(url: &str) -> bool {
    let path_part = url.split('?').next().unwrap_or(url);
    let lowercased = path_part.to_ascii_lowercase();
    [".mov", ".mp4", ".webm", ".m4v", ".mkv"]
        .iter()
        .any(|extension| lowercased.ends_with(extension))
}

/// The file name shown on a video embed card, falling back to the raw reference
/// when no file-name component can be extracted (for example a bare URL).
fn video_label(url: &str) -> SharedString {
    let path_part = url.split('?').next().unwrap_or(url);
    let label = Path::new(path_part)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path_part);
    SharedString::from(label.to_string())
}

/// Places a dropped image next to the document and returns the file name to use
/// in the inserted `![[...]]` wikilink. If the file already lives in the
/// document directory it is referenced in place; a name collision with a
/// different file produces a uniquely suffixed copy so nothing is overwritten.
fn prepare_dropped_image(document_dir: &Path, source: &Path) -> Option<String> {
    let file_name = source.file_name()?.to_string_lossy().to_string();
    let destination = document_dir.join(&file_name);

    if source == destination {
        return Some(file_name);
    }

    if !destination.exists() {
        std::fs::copy(source, &destination).log_err()?;
        return Some(file_name);
    }

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    let stem = source
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .unwrap_or_else(|| "image".to_string());
    let extension = source
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("png");
    let unique_name = format!("{}-{}.{}", stem, timestamp, extension);
    let unique_destination = document_dir.join(&unique_name);
    std::fs::copy(source, &unique_destination).log_err()?;
    Some(unique_name)
}

/// Maps a window-relative position (such as the mouse location at drop time) to
/// a multi-buffer anchor using the most recent paint layout. Returns `None`
/// when the position is outside the editor's text area or no layout is
/// available yet, in which case callers fall back to the current cursor.
fn anchor_for_window_position(
    editor: &Editor,
    position: gpui::Point<gpui::Pixels>,
) -> Option<Anchor> {
    let position_map = editor.last_position_map.as_ref()?;
    if !position_map.text_hitbox.contains(&position) {
        return None;
    }
    let display_point = position_map.point_for_position(position).previous_valid;
    Some(
        position_map
            .snapshot
            .display_point_to_anchor(display_point, text::Bias::Left),
    )
}

/// Attempts to handle image files dropped onto the editor when WYSIWYG mode is
/// active. Dropped images are stored alongside the document and inserted as
/// `![[file]]` wikilinks at the position the image was dropped. Returns true if
/// at least one image was inserted (so the pane skips its default "open file"
/// drop behavior).
pub fn try_handle_image_drop(
    editor: &Editor,
    editor_entity: Entity<Editor>,
    paths: &[PathBuf],
    window: &mut Window,
    cx: &mut App,
) -> bool {
    if !editor.markdown_wysiwyg_state.active {
        return false;
    }

    let Some(document_dir) = document_directory(editor, cx) else {
        return false;
    };

    let mut wikilinks: Vec<String> = Vec::new();
    for path in paths {
        if !is_image_path(path) {
            continue;
        }
        if let Some(file_name) = prepare_dropped_image(&document_dir, path) {
            wikilinks.push(format!("![[{}]]", file_name));
        }
    }

    if wikilinks.is_empty() {
        return false;
    }

    let insert_text = wikilinks.join("\n");
    // Resolve the drop location to a buffer anchor now, while the mouse is still
    // at the drop point and the paint layout is current.
    let drop_anchor = anchor_for_window_position(editor, window.mouse_position());
    // The drop is delivered while the editor entity is already being updated, so
    // defer the insertion until that update completes to avoid re-entrant access.
    window.defer(cx, move |window, cx| {
        editor_entity.update(cx, |editor, cx| {
            if let Some(drop_anchor) = drop_anchor {
                use crate::SelectionEffects;
                editor.change_selections(SelectionEffects::default(), window, cx, |selections| {
                    selections.select_anchor_ranges([drop_anchor..drop_anchor]);
                });
            }
            editor.insert(&insert_text, window, cx);
            schedule_wysiwyg_refresh(editor, cx);
        });
    });

    true
}
