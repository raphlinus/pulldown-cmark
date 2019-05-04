// Copyright 2017 Google Inc. All rights reserved.
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
// THE SOFTWARE.

//! Tree-based two pass parser.

use std::collections::{VecDeque, HashMap};
use std::ops::{Index, Range};
use std::cmp::min;

use unicase::UniCase;

use crate::strings::CowStr;
use crate::scanners::*;
use crate::tree::{TreePointer, TreeIndex, Tree};
use crate::linklabel::{scan_link_label, scan_link_label_rest, LinkLabel, ReferenceLabel};

#[derive(Clone, Debug, PartialEq)]
pub enum Tag<'a> {
    // block-level tags
    Paragraph,
    Rule,

    /// A heading. The field indicates the level of the heading.
    Header(i32),

    BlockQuote,
    CodeBlock(CowStr<'a>),

    /// A list. If the list is ordered the field indicates the number of the first item.
    List(Option<usize>),  // TODO: add delim and tight for ast (not needed for html)
    Item,
    FootnoteDefinition(CowStr<'a>),
    HtmlBlock,

    // tables
    Table(Vec<Alignment>),
    TableHead,
    TableRow,
    TableCell,

    // span-level tags
    Emphasis,
    Strong,
    Strikethrough,

    /// A link. The first field is the link type, the second the destination URL and the third is a title
    Link(LinkType, CowStr<'a>, CowStr<'a>),

    /// An image. The first field is the link type, the second the destination URL and the third is a title
    Image(LinkType, CowStr<'a>, CowStr<'a>),
}

#[derive(Clone, Debug, PartialEq, Copy)]
pub enum LinkType {
    /// Inline link like `[foo](bar)`
    Inline,
    /// Reference link like `[foo][bar]`
    Reference,
    /// Reference without destination in the document, but resolved by the broken_link_callback
    ReferenceUnknown,
    /// Collapsed link like `[foo][]`
    Collapsed,
    /// Collapsed link without destination in the document, but resolved by the broken_link_callback
    CollapsedUnknown,
    /// Shortcut link like `[foo]`
    Shortcut,
    /// Shortcut without destination in the document, but resolved by the broken_link_callback
    ShortcutUnknown,
    /// Autolink like `<http://foo.bar/baz>`
    Autolink,
    /// Email address in autolink like `<john@example.org>`
    Email,
}

impl LinkType {
    fn to_unknown(self) -> Self {
        match self {
            LinkType::Reference => LinkType::ReferenceUnknown,
            LinkType::Collapsed => LinkType::CollapsedUnknown,
            LinkType::Shortcut => LinkType::ShortcutUnknown,
            _ => unreachable!(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event<'a> {
    Start(Tag<'a>),
    End(Tag<'a>),
    Text(CowStr<'a>),
    Code(CowStr<'a>),
    Html(CowStr<'a>),
    InlineHtml(CowStr<'a>),
    FootnoteReference(CowStr<'a>),
    SoftBreak,
    HardBreak,
    /// A task list marker, rendered as a checkbox in HTML. Contains a true when it is checked
    TaskListMarker(bool),
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum Alignment {
    None,
    Left,
    Center,
    Right,
}

bitflags! {
    pub struct Options: u32 {
        const FIRST_PASS = 1 << 0;
        const ENABLE_TABLES = 1 << 1;
        const ENABLE_FOOTNOTES = 1 << 2;
        const ENABLE_STRIKETHROUGH = 1 << 3;
        const ENABLE_TASKLISTS = 1 << 4;
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct Item {
    start: usize,
    end: usize,
    body: ItemBody,
}

#[derive(Debug, PartialEq, Clone, Copy)]
enum ItemBody {
    Paragraph,
    Text,
    SoftBreak,
    HardBreak,

    // These are possible inline items, need to be resolved in second pass.

    // repeats, can_open, can_close
    MaybeEmphasis(usize, bool, bool),
    MaybeCode(usize), // number of backticks
    MaybeHtml,
    MaybeLinkOpen,
    MaybeLinkClose,
    MaybeImage,
    Backslash,

    // These are inline items after resolution.
    Emphasis,
    Strong,
    Strikethrough,
    Code(CowIndex),
    InlineHtml,
    Link(LinkIndex),
    Image(LinkIndex),
    FootnoteReference(CowIndex), 
    TaskListMarker(bool), // true for checked

    Rule,
    Header(i32), // header level
    FencedCodeBlock(CowIndex),
    IndentCodeBlock,
    HtmlBlock(Option<u32>), // end tag, or none for type 6
    Html,
    BlockQuote,
    List(bool, u8, usize), // is_tight, list character, list start index
    ListItem(usize), // indent level
    SynthesizeText(CowIndex),
    FootnoteDefinition(CowIndex),

    // Tables
    Table(AlignmentIndex),
    TableHead,
    TableRow,
    TableCell,

    // Dummy node at the top of the tree - should not be used otherwise!
    Root,
}

impl<'a> ItemBody {
    fn is_inline(&self) -> bool {
        match *self {
            ItemBody::MaybeEmphasis(..) | ItemBody::MaybeHtml | ItemBody::MaybeCode(_)
            | ItemBody::MaybeLinkOpen | ItemBody::MaybeLinkClose | ItemBody::MaybeImage => true,
            _ => false,
        }
    }
}

impl<'a> Default for ItemBody {
    fn default() -> Self {
        ItemBody::Root
    }
}

/// State for the first parsing pass.
///
/// The first pass resolves all block structure, generating an AST. Within a block, items
/// are in a linear chain with potential inline markup identified.
struct FirstPass<'a> {
    text: &'a str,
    tree: Tree<Item>,
    begin_list_item: bool,
    last_line_blank: bool,
    allocs: Allocations<'a>,
    options: Options,
}

impl<'a> FirstPass<'a> {
    fn new(text: &'a str, options: Options) -> FirstPass {
        // This is a very naive heuristic for the number of nodes
        // we'll need.
        let start_capacity = std::cmp::max(128, text.len() / 32);
        let tree = Tree::with_capacity(start_capacity);
        let begin_list_item = false;
        let last_line_blank = false;
        let allocs = Allocations::new();
        FirstPass { text, tree, begin_list_item, last_line_blank, allocs, options }
    }

    fn run(mut self) -> (Tree<Item>, Allocations<'a>) {
        let mut ix = 0;
        while ix < self.text.len() {
            ix = self.parse_block(ix);
        }
        for _ in 0..self.tree.spine_len() {
            self.pop(ix);
        }
        (self.tree, self.allocs)
    }

    /// Returns offset after block.
    fn parse_block(&mut self, mut start_ix: usize) -> usize {
        let bytes = self.text.as_bytes();
        let mut line_start = LineStart::new(&bytes[start_ix..]);

        let i = self.scan_containers(&mut line_start);
        for _ in i..self.tree.spine_len() {
            self.pop(start_ix);
        }

        // finish footnote if it's still open and was preceeded by blank line
        if let Some(node_ix) = self.tree.peek_up() {
            if let ItemBody::FootnoteDefinition(..) = self.tree[node_ix].item.body {
                if self.last_line_blank {
                    self.pop(start_ix);
                }
            }
        }
        
        if self.options.contains(Options::ENABLE_FOOTNOTES) {
            // Footnote definitions of the form
            // [^bar]:
            // * anything really
            let container_start = start_ix + line_start.bytes_scanned();
            if let Some(bytecount) = self.parse_footnote(container_start) {
                start_ix = container_start + bytecount;
                start_ix += scan_blank_line(&bytes[start_ix..]).unwrap_or(0);
                line_start = LineStart::new(&bytes[start_ix..]);      
            }
        }

        // Process new containers
        loop {
            let container_start = start_ix + line_start.bytes_scanned();
            if line_start.scan_blockquote_marker() {
                self.finish_list(start_ix);
                self.tree.append(Item {
                    start: container_start,
                    end: 0, // will get set later
                    body: ItemBody::BlockQuote,
                });
                self.tree.push();
            } else if let Some((ch, index, indent)) = line_start.scan_list_marker() {
                let after_marker_index = start_ix + line_start.bytes_scanned();
                self.continue_list(container_start, ch, index);
                self.tree.append(Item {
                    start: container_start,
                    end: after_marker_index, // will get updated later if item not empty
                    body: ItemBody::ListItem(indent),
                });
                self.tree.push();
                if let Some(n) = scan_blank_line(&bytes[after_marker_index..]) {
                    self.begin_list_item = true;
                    return after_marker_index + n;
                }
                if self.options.contains(Options::ENABLE_TASKLISTS) {
                    if let Some(is_checked) = line_start.scan_task_list_marker() {
                        self.tree.append(Item {
                            start: after_marker_index,
                            end: start_ix + line_start.bytes_scanned(),
                            body: ItemBody::TaskListMarker(is_checked),
                        });
                    }
                }
            }
            else {
                break;
            }
        }

        let ix = start_ix + line_start.bytes_scanned();

        if let Some(n) = scan_blank_line(&bytes[ix..]) {
            if let Some(node_ix) = self.tree.peek_up() {
                match self.tree[node_ix].item.body {
                    ItemBody::BlockQuote => (),
                    _ => {
                        if self.begin_list_item {
                            // A list item can begin with at most one blank line.
                            self.pop(start_ix);
                        }
                        self.last_line_blank = true;
                    }
                }
            }
            return ix + n;
        }

        self.begin_list_item = false;
        self.finish_list(start_ix);

        // Save `remaining_space` here to avoid needing to backtrack `line_start` for HTML blocks
        let remaining_space = line_start.remaining_space();

        let indent = line_start.scan_space_upto(4);
        if indent == 4 {
            let ix = start_ix + line_start.bytes_scanned();
            let remaining_space = line_start.remaining_space();
            return self.parse_indented_code_block(ix, remaining_space);
        }


        // HTML Blocks

        // Start scanning at the first nonspace character, but don't advance `ix` yet because any
        // spaces present before the HTML block begins should be preserved.
        let nonspace_ix = start_ix + line_start.bytes_scanned();

        if self.text.as_bytes()[nonspace_ix] == b'<' {
            // Types 1-5 are all detected by one function and all end with the same
            // pattern
            if let Some(html_end_tag_ix) = get_html_end_tag(&bytes[nonspace_ix..]) {
                return self.parse_html_block_type_1_to_5(ix, html_end_tag_ix, remaining_space);
            }

            // Detect type 6
            let possible_tag = scan_html_block_tag(&bytes[nonspace_ix..]).1;
            if is_html_tag(possible_tag) {
                return self.parse_html_block_type_6_or_7(ix, remaining_space);
            }

            // Detect type 7
            if let Some(_html_bytes) = scan_html_type_7(&bytes[nonspace_ix..]) {
                return self.parse_html_block_type_6_or_7(ix, remaining_space);
            }
        }

        // Advance `ix` after HTML blocks have been scanned
        let ix = start_ix + line_start.bytes_scanned();

        if let Ok(n) = scan_hrule(&bytes[ix..]) {
            return self.parse_hrule(n, ix);
        }

        if let Some((atx_size, atx_level)) = scan_atx_heading(&bytes[ix..]) {
            return self.parse_atx_heading(ix, atx_level, atx_size);
        }

        // parse refdef
        if let Some((bytecount, label, link_def)) = self.parse_refdef_total(ix) {
            self.allocs.refdefs.entry(label).or_insert(link_def);
            return ix + bytecount;
        }

        if let Some((n, fence_ch)) = scan_code_fence(&bytes[ix..]) {
            return self.parse_fenced_code_block(ix, indent, fence_ch, n);
        }
        self.parse_paragraph(ix)
    }

    /// Returns the offset of the first line after the table.
    /// Assumptions: current focus is a table element and the table header
    /// matches the separator line (same number of columns).
    fn parse_table(&mut self, table_cols: usize, head_start: usize, body_start: usize) -> usize {
        // parse header. this shouldn't fail because we (should have) made sure the table
        // header is ok
        let (_sep_start, thead_ix) = self.parse_table_row(head_start, table_cols).unwrap();
        self.tree[thead_ix].item.body = ItemBody::TableHead;

        // parse body
        let mut ix = body_start;
        while let Some((next_ix, _row_ix)) = self.parse_table_row(ix, table_cols) {
            ix = next_ix;
        }

        self.pop(ix);
        ix
    }

    /// Returns first offset after the row and the tree index of the row.
    fn parse_table_row(&mut self, mut ix: usize, row_cells: usize) -> Option<(usize, TreeIndex)> {
        let bytes = self.text.as_bytes();
        let mut line_start = LineStart::new(&bytes[ix..]);
        let _n_containers = self.scan_containers(&mut line_start);
        let mut cells = 0;
        let mut final_cell_ix = None;
        ix += line_start.bytes_scanned();

        if scan_paragraph_interrupt(&bytes[ix..]) {
            return None;
        }

        let row_ix = self.tree.append(Item {
            start: ix,
            end: 0, // set at end of this function
            body: ItemBody::TableRow,
        });
        self.tree.push();

        loop {
            ix += scan_ch(&bytes[ix..], b'|');
            ix += scan_whitespace_no_nl(&bytes[ix..]);
            
            if let Some(eol_bytes) = scan_eol(&bytes[ix..]) {
                ix += eol_bytes;
                break;
            }

            let cell_ix = self.tree.append(Item {
                start: ix,
                end: ix,
                body: ItemBody::TableCell,
            });
            self.tree.push();
            let (next_ix, _brk) = self.parse_line(ix, true);
            self.tree[cell_ix].item.end = next_ix;
            self.tree.pop();

            ix = next_ix;
            cells += 1;

            if cells == row_cells {
                final_cell_ix = Some(cell_ix);
            }
        }

        // fill empty cells if needed
        // note: this is where GFM and commonmark-extra diverge. we follow
        // GFM here
        for _ in cells..row_cells {
            self.tree.append(Item {
                start: ix,
                end: ix,
                body: ItemBody::TableCell,
            });
        }

        // drop excess cells
        if let Some(cell_ix) = final_cell_ix {
            self.tree[cell_ix].next = TreePointer::Nil;
        }

        self.pop(ix);
        Some((ix, row_ix))
    }

    /// Returns offset of line start after paragraph.
    fn parse_paragraph(&mut self, start_ix: usize) -> usize {
        let node_ix = self.tree.append(Item {
            start: start_ix,
            end: 0,  // will get set later
            body: ItemBody::Paragraph,
        });
        self.tree.push();
        let bytes = self.text.as_bytes();

        let mut ix = start_ix;
        loop {
            let (next_ix, brk) = self.parse_line(ix, false);

            // break out when we find a table
            if let Some(Item { body: ItemBody::Table(alignment_ix), start, end }) = brk {
                let table_cols = self.allocs[alignment_ix].len();
                self.tree[node_ix].item = Item { body: ItemBody::Table(alignment_ix), start, end };
                // this clears out any stuff we may have appended - but there may
                // be a cleaner way
                self.tree[node_ix].child = TreePointer::Nil;
                self.tree.pop();
                self.tree.push();
                return self.parse_table(table_cols, ix, next_ix);
            }

            ix = next_ix;
            let mut line_start = LineStart::new(&bytes[ix..]);
            let n_containers = self.scan_containers(&mut line_start);
            if !line_start.scan_space(4) {
                let ix_new = ix + line_start.bytes_scanned();
                if n_containers == self.tree.spine_len() {
                    if let Some((n, level)) = scan_setext_heading(&bytes[ix_new..]) {
                        self.tree[node_ix].item.body = ItemBody::Header(level);
                        if let Some(Item { start, body: ItemBody::HardBreak, .. }) = brk {
                            if bytes[start] == b'\\' {
                                self.tree.append_text(start, start + 1);
                            }
                        }
                        ix = ix_new + n;
                        break;
                    }
                }
                // first check for non-empty lists, then for other interrupts    
                let suffix = &bytes[ix_new..];
                if self.interrupt_paragraph_by_list(suffix) || scan_paragraph_interrupt(suffix) {
                    break;
                }
            }
            line_start.scan_all_space();
            ix = next_ix + line_start.bytes_scanned();
            if let Some(item) = brk {
                self.tree.append(item);
            }
        }

        self.pop(ix);
        ix
    }

    /// Parse a line of input, appending text and items to tree.
    ///
    /// Returns: index after line and an item representing the break.
    fn parse_line(&mut self, start: usize, inside_table: bool) -> (usize, Option<Item>) {
        let bytes = &self.text.as_bytes();
        let mut pipes = 0;
        let mut last_pipe_ix = start;
        let mut begin_text = start;

        let (final_ix, brk) = iterate_special_bytes(bytes, start, |ix, byte| {
            match byte {
                b'\n' | b'\r' => {
                    if inside_table {
                        return LoopInstruction::BreakAtWith(ix, None);
                    }

                    let mut i = ix;
                    let eol_bytes = scan_eol(&bytes[ix..]).unwrap_or(0);
                    let end_ix = ix + eol_bytes;
                    if bytes.get(ix - 1) == Some(&b'\\') && end_ix < self.text.len() {
                        i -= 1;
                        self.tree.append_text(begin_text, i);
                        return LoopInstruction::BreakAtWith(end_ix, Some(Item {
                            start: i,
                            end: end_ix,
                            body: ItemBody::HardBreak,
                        }));
                    }
                    let trailing_whitespace = bytes[..ix].iter()
                        .rev()
                        .take_while(|&&b| is_ascii_whitespace_no_nl(b))
                        .count();
                    if trailing_whitespace >= 2 {
                        i -= trailing_whitespace;
                        self.tree.append_text(begin_text, i);
                        return LoopInstruction::BreakAtWith(end_ix, Some(Item {
                            start: i,
                            end: end_ix,
                            body: ItemBody::HardBreak,
                        }));
                    }
                    if self.options.contains(Options::ENABLE_TABLES) && !inside_table && pipes > 0 {
                        // check if we may be parsing a table
                        let next_line_ix = ix + eol_bytes;
                        let mut line_start = LineStart::new(&bytes[next_line_ix..]);
                        let _n_containers = self.scan_containers(&mut line_start);
                        let table_head_ix = next_line_ix + line_start.bytes_scanned();
                        let (table_head_bytes, alignment) = scan_table_head(&bytes[table_head_ix..]);

                        if table_head_bytes > 0 {
                            // computing header count from number of pipes
                            let header_count = count_header_cols(bytes, pipes, start, last_pipe_ix);

                            // make sure they match the number of columns we find in separator line
                            if alignment.len() == header_count {
                                let alignment_ix = self.allocs.allocate_alignment(alignment);
                                let end_ix = table_head_ix + table_head_bytes;
                                return LoopInstruction::BreakAtWith(end_ix, Some(Item {
                                    start: i,
                                    end: end_ix, // must update later
                                    body: ItemBody::Table(alignment_ix),
                                }));
                            }
                        }
                    }
                        
                    self.tree.append_text(begin_text, ix);
                    LoopInstruction::BreakAtWith(end_ix, Some(Item {
                        start: i,
                        end: end_ix,
                        body: ItemBody::SoftBreak,
                    }))
                }
                b'\\' => {
                    if ix + 1 < self.text.len() && is_ascii_punctuation(bytes[ix + 1]) {
                        self.tree.append_text(begin_text, ix);
                        if !inside_table || bytes[ix + 1] != b'|' {
                            self.tree.append(Item {
                                start: ix,
                                end: ix + 1,
                                body: ItemBody::Backslash,
                            });
                        }
                        begin_text = ix + 1;
                        LoopInstruction::ContinueAndSkip(if bytes[ix + 1] == b'`' { 0 } else { 1 })
                    } else {
                        LoopInstruction::ContinueAndSkip(0)
                    }
                }
                c @ b'*' | c @ b'_' | c @ b'~' => {
                    let string_suffix = &self.text[ix..];
                    let count = 1 + scan_ch_repeat(&string_suffix.as_bytes()[1..], c);
                    let can_open = delim_run_can_open(&self.text, string_suffix, count, ix);
                    let can_close = delim_run_can_close(&self.text, string_suffix, count, ix);
                    let is_valid_seq = c != b'~' || count == 2 && self.options.contains(Options::ENABLE_STRIKETHROUGH);

                    if (can_open || can_close) && is_valid_seq {
                        self.tree.append_text(begin_text, ix);
                        for i in 0..count {
                            self.tree.append(Item {
                                start: ix + i,
                                end: ix + i + 1,
                                body: ItemBody::MaybeEmphasis(count - i, can_open, can_close),
                            });
                        }
                        begin_text = ix + count;
                    }
                    LoopInstruction::ContinueAndSkip(count - 1)
                }
                b'`' => {
                    self.tree.append_text(begin_text, ix);
                    let count = 1 + scan_ch_repeat(&bytes[ix+1..], b'`');
                    self.tree.append(Item {
                        start: ix,
                        end: ix + count,
                        body: ItemBody::MaybeCode(count),
                    });
                    begin_text = ix + count;
                    LoopInstruction::ContinueAndSkip(count - 1)
                }
                b'<' => {
                    // Note: could detect some non-HTML cases and early escape here, but not
                    // clear that's a win.
                    self.tree.append_text(begin_text, ix);
                    self.tree.append(Item {
                        start: ix,
                        end: ix + 1,
                        body: ItemBody::MaybeHtml,
                    });
                    begin_text = ix + 1;
                    LoopInstruction::ContinueAndSkip(0)
                }
                b'!' => {
                    if ix + 1 < self.text.len() && bytes[ix + 1] == b'[' {
                        self.tree.append_text(begin_text, ix);
                        self.tree.append(Item {
                            start: ix,
                            end: ix + 2,
                            body: ItemBody::MaybeImage,
                        });
                        begin_text = ix + 2;
                        LoopInstruction::ContinueAndSkip(1)
                    } else {
                        LoopInstruction::ContinueAndSkip(0)
                    }
                }
                b'[' => {
                    self.tree.append_text(begin_text, ix);
                    self.tree.append(Item {
                        start: ix,
                        end: ix + 1,
                        body: ItemBody::MaybeLinkOpen,
                    });
                    begin_text = ix + 1;
                    LoopInstruction::ContinueAndSkip(0)
                }
                b']' => {
                    self.tree.append_text(begin_text, ix);
                    self.tree.append(Item {
                        start: ix,
                        end: ix + 1,
                        body: ItemBody::MaybeLinkClose,
                    });
                    begin_text = ix + 1;
                    LoopInstruction::ContinueAndSkip(0)
                }
                b'&' => {
                    match scan_entity(&bytes[ix..]) {
                        (n, Some(value)) => {
                            self.tree.append_text(begin_text, ix);
                            self.tree.append(Item {
                                start: ix,
                                end: ix + n,
                                body: ItemBody::SynthesizeText(self.allocs.allocate_cow(value)),
                            });
                            begin_text = ix + n;
                            LoopInstruction::ContinueAndSkip(n - 1)
                        }
                        _ => LoopInstruction::ContinueAndSkip(0),
                    }
                }
                b'|' => {
                    if inside_table {
                        LoopInstruction::BreakAtWith(ix, None)
                    } else {
                        last_pipe_ix = ix;
                        pipes += 1;
                        LoopInstruction::ContinueAndSkip(0)
                    }
                }
                _ => LoopInstruction::ContinueAndSkip(0),
            }
        });

        if brk.is_none() {
            // need to close text at eof
            self.tree.append_text(begin_text, final_ix);
        }
        (final_ix, brk)
    }

    /// Check whether we should allow a paragraph interrupt by lists. Only non-empty
    /// lists are allowed.
    fn interrupt_paragraph_by_list(&self, suffix: &[u8]) -> bool {
        let (ix, delim, index, _) = scan_listitem(suffix);

        if ix == 0 {
            return false;
        }

        // we don't allow interruption by either empty lists or
        // numbered lists starting at an index other than 1
        if !scan_empty_list(&suffix[ix..]) && (delim == b'*' || delim == b'-' || index == 1) {
            return true;
        }

        // check if we are currently in a list
        self.tree.peek_grandparent().map_or(false, |gp_ix| {
            match self.tree[gp_ix].item.body {
                ItemBody::ListItem(..) => true,
                _ => false,
            }
        })
    }

    /// When start_ix is at the beginning of an HTML block of type 1 to 5,
    /// this will find the end of the block, adding the block itself to the
    /// tree and also keeping track of the lines of HTML within the block.
    ///
    /// The html_end_tag is the tag that must be found on a line to end the block.
    fn parse_html_block_type_1_to_5(&mut self, start_ix: usize, html_end_tag_ix: u32,
            mut remaining_space: usize) -> usize
    {
        self.tree.append(Item {
            start: start_ix,
            end: 0, // set later
            body: ItemBody::HtmlBlock(Some(html_end_tag_ix)),
        });
        self.tree.push();

        let bytes = self.text.as_bytes();
        let mut ix = start_ix;
        let end_ix;
        loop {
            let line_start_ix = ix;
            ix += scan_nextline(&bytes[ix..]);
            self.append_html_line(remaining_space, line_start_ix, ix);

            let mut line_start = LineStart::new(&bytes[ix..]);
            let n_containers = self.scan_containers(&mut line_start);
            if n_containers < self.tree.spine_len() {
                end_ix = ix;
                break;
            }

            let html_end_tag = HTML_END_TAGS[html_end_tag_ix as usize];

            if (&self.text[line_start_ix..ix]).contains(html_end_tag) {
                end_ix = ix;
                break;
            }

            let next_line_ix = ix + line_start.bytes_scanned();
            if next_line_ix == self.text.len() {
                end_ix = next_line_ix;
                break;
            }
            ix = next_line_ix;
            remaining_space = line_start.remaining_space();
        }
        self.pop(end_ix);
        ix
    }

    /// When start_ix is at the beginning of an HTML block of type 6 or 7,
    /// this will consume lines until there is a blank line and keep track of
    /// the HTML within the block.
    fn parse_html_block_type_6_or_7(&mut self, start_ix: usize, mut remaining_space: usize)
        -> usize
    {
        self.tree.append(Item {
            start: start_ix,
            end: 0, // set later
            body: ItemBody::HtmlBlock(None)
        });
        self.tree.push();

        let bytes = self.text.as_bytes();
        let mut ix = start_ix;
        let end_ix;
        loop {
            let line_start_ix = ix;
            ix += scan_nextline(&bytes[ix..]);
            self.append_html_line(remaining_space, line_start_ix, ix);

            let mut line_start = LineStart::new(&bytes[ix..]);
            let n_containers = self.scan_containers(&mut line_start);
            if n_containers < self.tree.spine_len() || line_start.is_at_eol()
            {
                end_ix = ix;
                break;
            }

            let next_line_ix = ix + line_start.bytes_scanned();
            if next_line_ix == self.text.len()
                || scan_blank_line(&bytes[next_line_ix..]).is_some()
            {
                end_ix = next_line_ix;
                break;
            }
            ix = next_line_ix;
            remaining_space = line_start.remaining_space();
        }
        self.pop(end_ix);
        ix
    }

    fn parse_indented_code_block(&mut self, start_ix: usize, mut remaining_space: usize)
        -> usize
    {
        self.tree.append(Item {
            start: start_ix,
            end: 0,  // will get set later
            body: ItemBody::IndentCodeBlock,
        });
        self.tree.push();
        let bytes = self.text.as_bytes();
        let mut last_nonblank_child = TreePointer::Nil;
        let mut last_nonblank_ix = 0;
        let mut end_ix = 0;
        let mut last_line_blank = false;

        let mut ix = start_ix;
        loop {
            let line_start_ix = ix;
            ix += scan_nextline(&bytes[ix..]);
            self.append_code_text(remaining_space, line_start_ix, ix);
            // TODO(spec clarification): should we synthesize newline at EOF?

            if !last_line_blank {
                last_nonblank_child = self.tree.cur();
                last_nonblank_ix = ix;
                end_ix = ix;
            }

            let mut line_start = LineStart::new(&bytes[ix..]);
            let n_containers = self.scan_containers(&mut line_start);
            if n_containers < self.tree.spine_len()
                || !(line_start.scan_space(4) || line_start.is_at_eol())
            {
                break;
            }
            let next_line_ix = ix + line_start.bytes_scanned();
            if next_line_ix == self.text.len() {
                break;
            }
            ix = next_line_ix;
            remaining_space = line_start.remaining_space();
            last_line_blank = scan_blank_line(&bytes[ix..]).is_some();
        }

        // Trim trailing blank lines.
        if let TreePointer::Valid(child) = last_nonblank_child {
            self.tree[child].next = TreePointer::Nil;
            self.tree[child].item.end = last_nonblank_ix;
        }
        self.pop(end_ix);
        ix
    }

    fn parse_fenced_code_block(&mut self, start_ix: usize, indent: usize,
        fence_ch: u8, n_fence_char: usize) -> usize
    {
        let bytes = self.text.as_bytes();
        let mut info_start = start_ix + n_fence_char;
        info_start += scan_whitespace_no_nl(&bytes[info_start..]);
        // TODO: info strings are typically very short. wouldnt it be faster
        // to just do a forward scan here?
        let mut ix = info_start + scan_nextline(&bytes[info_start..]);
        let info_end = ix - bytes[info_start..ix].iter()
            .rev()
            .take_while(|&&b| is_ascii_whitespace(b))
            .count();
        let info_string = unescape(&self.text[info_start..info_end]);
        self.tree.append(Item {
            start: start_ix,
            end: 0,  // will get set later
            body: ItemBody::FencedCodeBlock(self.allocs.allocate_cow(info_string)),
        });
        self.tree.push();
        loop {
            let mut line_start = LineStart::new(&bytes[ix..]);
            let n_containers = self.scan_containers(&mut line_start);
            if n_containers < self.tree.spine_len() {
                break;
            }
            line_start.scan_space(indent);
            let mut close_line_start = line_start.clone();
            if !close_line_start.scan_space(4) {
                let close_ix = ix + close_line_start.bytes_scanned();
                if let Some(n) =
                    scan_closing_code_fence(&bytes[close_ix..], fence_ch, n_fence_char)
                {
                    ix = close_ix + n;
                    break;
                }
            }
            let remaining_space = line_start.remaining_space();
            ix += line_start.bytes_scanned();
            let next_ix = ix + scan_nextline(&bytes[ix..]);
            self.append_code_text(remaining_space, ix, next_ix);
            ix = next_ix;
        }

        self.pop(ix);

        // try to read trailing whitespace or it will register as a completely blank line
        ix + scan_blank_line(&bytes[ix..]).unwrap_or(0)
    }

    fn append_code_text(&mut self, remaining_space: usize, start: usize, end: usize) {
        if remaining_space > 0 {
            let cow_ix = self.allocs.allocate_cow("   "[..remaining_space].into());
            self.tree.append(Item {
                start,
                end: start,
                body: ItemBody::SynthesizeText(cow_ix),
            });
        }
        if self.text.as_bytes()[end - 2] == b'\r' {
            // Normalize CRLF to LF
            self.tree.append_text(start, end - 2);
            self.tree.append_text(end - 1, end);
        } else {
            self.tree.append_text(start, end);
        }
    }


    /// Appends a line of HTML to the tree.
    fn append_html_line(&mut self, remaining_space: usize, start: usize, end: usize) {
        if remaining_space > 0 {
            let cow_ix = self.allocs.allocate_cow("   "[..remaining_space].into());
            self.tree.append(Item {
                start,
                end: start,
                // TODO: maybe this should synthesize to html rather than text?
                body: ItemBody::SynthesizeText(cow_ix),
            });
        }
        if self.text.as_bytes()[end - 2] == b'\r' {
            // Normalize CRLF to LF
            self.tree.append(Item {
                start,
                end: end - 2,
                body: ItemBody::Html,
            });
            self.tree.append(Item {
                start: end - 1,
                end,
                body: ItemBody::Html,
            });
        } else {
            self.tree.append(Item {
                start,
                end,
                body: ItemBody::Html,
            });
        }
    }

    /// Returns number of containers scanned.
    fn scan_containers(&self, line_start: &mut LineStart) -> usize {
        let mut i = 0;
        for &node_ix in self.tree.walk_spine() {
            match self.tree[node_ix].item.body {
                ItemBody::BlockQuote => {
                    let save = line_start.clone();
                    if !line_start.scan_blockquote_marker() {
                        *line_start = save;
                        break;
                    }
                }
                ItemBody::ListItem(indent) => {
                    if !line_start.is_at_eol() {
                        let save = line_start.clone();
                        if !line_start.scan_space(indent){
                            *line_start = save;
                            break;
                        }
                    }
                }
                _ => (),
            }
            i += 1;
        }
        i
    }

    /// Pop a container, setting its end.
    fn pop(&mut self, ix: usize) {
        let cur_ix = self.tree.pop().unwrap();
        self.tree[cur_ix].item.end = ix;
        if let ItemBody::List(true, _, _) = self.tree[cur_ix].item.body {
            surgerize_tight_list(&mut self.tree, cur_ix);
        }
    }

    /// Close a list if it's open. Also set loose if last line was blank
    fn finish_list(&mut self, ix: usize) {
        if let Some(node_ix) = self.tree.peek_up() {
            if let ItemBody::List(_, _, _) = self.tree[node_ix].item.body {
                self.pop(ix);
            }
        }
        if self.last_line_blank {
            if let Some(node_ix) = self.tree.peek_grandparent() {
                if let ItemBody::List(ref mut is_tight, _, _) =
                    self.tree[node_ix].item.body
                {
                    *is_tight = false;
                }
            }
            self.last_line_blank = false;
        }
    }

    /// Continue an existing list or start a new one if there's not an open
    /// list that matches.
    fn continue_list(&mut self, start: usize, ch: u8, index: usize) {
        if let Some(node_ix) = self.tree.peek_up() {
            if let ItemBody::List(ref mut is_tight, existing_ch, _) =
                self.tree[node_ix].item.body
            {
                if existing_ch == ch {
                    if self.last_line_blank {
                        *is_tight = false;
                        self.last_line_blank = false;
                    }
                    return;
                }
            }
            // TODO: this is not the best choice for end; maybe get end from last list item.
            self.finish_list(start);
        }
        self.tree.append(Item {
            start,
            end: 0,  // will get set later
            body: ItemBody::List(true, ch, index),
        });
        self.tree.push();
        self.last_line_blank = false;
    }

    /// Parse a thematic break.
    ///
    /// Returns index of start of next line.
    fn parse_hrule(&mut self, hrule_size: usize, ix: usize) -> usize {
        self.tree.append(Item {
            start: ix,
            end: ix + hrule_size,
            body: ItemBody::Rule,
        });
        ix + hrule_size
    }

    /// Parse an ATX heading.
    ///
    /// Returns index of start of next line.
    fn parse_atx_heading(&mut self, mut ix: usize, atx_level: i32, atx_size: usize) -> usize {
        self.tree.append(Item {
            start: ix,
            end: 0, // set later
            body: ItemBody::Header(atx_level),
        });
        ix += atx_size;
        // next char is space or scan_eol
        // (guaranteed by scan_atx_heading)
        let bytes = self.text.as_bytes();
        let b = bytes[ix];
        if b == b'\n' || b == b'\r' {
            return ix + scan_eol(&bytes[ix..]).unwrap_or(0);
        }
        // skip leading spaces
        let skip_spaces = scan_whitespace_no_nl(&bytes[ix..]);
        ix += skip_spaces;

        // now handle the header text
        let header_start = ix;
        let header_node_idx = self.tree.push(); // so that we can set the endpoint later
        ix = self.parse_line(ix, false).0;
        self.tree[header_node_idx].item.end = ix;

        // remove trailing matter from header text
        if let TreePointer::Valid(cur_ix) = self.tree.cur() {
            let header_text = &bytes[header_start..ix];
            let mut limit = header_text.iter()
                .rposition(|&b| !(b == b'\n' || b == b'\r' || b == b' '))
                .map_or(0, |i| i + 1);
            let closer = header_text[..limit].iter()
                .rposition(|&b| b != b'#')
                .map_or(0, |i| i + 1);
            if closer == 0 {
                limit = closer;
            } else {
                let spaces = header_text[..closer].iter()
                    .rev()
                    .take_while(|&&b| b == b' ')
                    .count();
                if spaces > 0 {
                    limit = closer - spaces;
                }
            }
            self.tree[cur_ix].item.end = limit + header_start;
        }

        self.tree.pop();
        ix
    }

    /// Returns the number of bytes scanned on success.
    fn parse_footnote(&mut self, start: usize) -> Option<usize> {
        let bytes = &self.text.as_bytes()[start..];
        if !bytes.starts_with(b"[^") {
            return None;
        }
        let (mut i, label) = scan_link_label_rest(&self.text[(start + 2)..])?;
        i += 2;
        if scan_ch(&bytes[i..], b':') == 0 {
            return None;
        }
        i += 1;
        self.tree.append(Item {
            start,
            end: 0,  // will get set later
            body: ItemBody::FootnoteDefinition(self.allocs.allocate_cow(label)), // TODO: check whether the label here is strictly necessary
        });
        self.tree.push();
        Some(i)
    }

    /// Returns number of bytes scanned, label and definition on success.
    fn parse_refdef_total(&mut self, start: usize) -> Option<(usize, LinkLabel<'a>, LinkDef<'a>)> {
        let bytes = &self.text.as_bytes()[start..];
        if scan_ch(bytes, b'[') == 0 {
            return None;
        }
        let (mut i, label) = scan_link_label_rest(&self.text[(start + 1)..])?;
        i += 1;
        if scan_ch(&bytes[i..], b':') == 0 {
            return None;
        }
        i += 1;
        let (bytecount, link_def) = self.scan_refdef(start + i)?;
        Some((bytecount + i, UniCase::new(label), link_def))
    }

    /// Returns # of bytes and definition.
    /// Assumes the label of the reference including colon has already been scanned.
    fn scan_refdef(&self, start: usize) -> Option<(usize, LinkDef<'a>)> {
        let mut i = start;
        let bytes = self.text.as_bytes();

        // whitespace between label and url (including up to one newline)
        let mut newlines = 0;
        for &c in &bytes[i..] {
            if c == b'\n' {
                i += 1;
                newlines += 1;
                if newlines > 1 {
                    return None;
                } else {
                    let mut line_start = LineStart::new(&bytes[i..]);
                    self.scan_containers(&mut line_start);
                }
            } else if is_ascii_whitespace_no_nl(c) {
                i += 1;
            } else {
                break;
            }
        }

        // scan link dest
        let (dest_length, dest) = scan_link_dest(&self.text, i, 1)?;
        let dest = unescape(dest);
        i += dest_length;

        // scan whitespace between dest and label
        // FIXME: dedup with similar block above
        newlines = 0;
        let mut whitespace_bytes = 0;
        for &c in &bytes[i..] {
            if c == b'\n' {
                whitespace_bytes += 1;
                newlines += 1;
                let mut line_start = LineStart::new(&bytes[(i + whitespace_bytes)..]);
                let _n_containers = self.scan_containers(&mut line_start);
            } else if is_ascii_whitespace_no_nl(c) {
                whitespace_bytes += 1;
            } else {
                break;
            }
        }
        if i == self.text.len() {
            newlines += 1;
        }
        if whitespace_bytes == 0 && newlines == 0 {
            return None;
        }

        // no title
        let mut backup = 
            (i - start,
            LinkDef {
                dest,
                title: None,
            });

        if newlines > 1 {
            return Some(backup);
        } else {
            i += whitespace_bytes;
        }        

        // scan title
        // if this fails but newline == 1, return also a refdef without title
        if let Some((title_length, title)) = scan_refdef_title(&self.text[i..]) {
            i += title_length;
            backup.1.title = Some(unescape(title));
        } else if newlines > 0 {
            return Some(backup);
        } else {
            return None;
        };

        // scan EOL
        if let Some(bytes) = scan_blank_line(&bytes[i..]) {
            backup.0 = i + bytes - start;
            Some(backup)
        } else if newlines > 0 {
            Some(backup)
        } else {
            None
        }
    }
}

/// Computes the number of header columns in a table line by computing the number of dividing pipes
/// that aren't followed or preceeded by whitespace.
fn count_header_cols(bytes: &[u8], mut pipes: usize, mut start: usize, last_pipe_ix: usize) -> usize {
    // was first pipe preceeded by whitespace? if so, subtract one
    start += scan_whitespace_no_nl(&bytes[start..]);
    if bytes[start] == b'|' {
        pipes -= 1;
    }

    // was last pipe followed by whitespace? if so, sub one
    if scan_blank_line(&bytes[(last_pipe_ix + 1)..]).is_some() {
        pipes -= 1;
    }

    pipes + 1
}

impl<'a> Tree<Item> {
    fn append_text(&mut self, start: usize, end: usize) {
        if end > start {
            if let TreePointer::Valid(ix) = self.cur() {
                if ItemBody::Text == self[ix].item.body && self[ix].item.end == start {
                    self[ix].item.end = end;
                    return;
                }
            }
            self.append(Item {
                start,
                end,
                body: ItemBody::Text,
            });
        }
    }
}

/// Determines whether the delimiter run starting at given index is
/// left-flanking, as defined by the commonmark spec (and isn't intraword
/// for _ delims).
/// suffix is &s[ix..], which is passed in as an optimization, since taking
/// a string subslice is O(n).
fn delim_run_can_open(s: &str, suffix: &str, run_len: usize, ix: usize) -> bool {
    let next_char = if let Some(c) = suffix.chars().nth(run_len) {
        c
    } else {
        return false;
    };
    if next_char.is_whitespace() {
        return false;
    }
    if ix == 0 {
        return true;
    }
    let delim = suffix.chars().next().unwrap();
    if delim == '*' && !is_punctuation(next_char) {
        return true;
    }

    let prev_char = s[..ix].chars().last().unwrap();

    prev_char.is_whitespace() || is_punctuation(prev_char)
}

/// Determines whether the delimiter run starting at given index is
/// left-flanking, as defined by the commonmark spec (and isn't intraword
/// for _ delims)
fn delim_run_can_close(s: &str, suffix: &str, run_len: usize, ix: usize) -> bool {
    if ix == 0 {
        return false;
    }
    let prev_char = s[..ix].chars().last().unwrap();
    if prev_char.is_whitespace() {
        return false;
    }
    let next_char = if let Some(c) = suffix.chars().nth(run_len) {
        c
    } else {
        return true;
    };
    let delim = suffix.chars().next().unwrap();
    if delim == '*' && !is_punctuation(prev_char) {
        return true;
    }

    next_char.is_whitespace() || is_punctuation(next_char)
}

/// Checks whether we should break a paragraph on the given input.
/// Note: lists are dealt with in `interrupt_paragraph_by_list`, because determing
/// whether to break on a list requires additional context.
fn scan_paragraph_interrupt(bytes: &[u8]) -> bool {
    scan_eol(bytes).is_some() ||
    scan_hrule(bytes).is_ok() ||
    scan_atx_heading(bytes).is_some() ||
    scan_code_fence(bytes).is_some() ||
    get_html_end_tag(bytes).is_some() ||
    scan_blockquote_start(bytes).is_some() ||
    is_html_tag(scan_html_block_tag(bytes).1)
}

static HTML_END_TAGS: &[&str; 7] = &["</pre>", "</style>", "</script>", "-->", "?>", "]]>", ">"];

// Returns an index into HTML_END_TAGS
fn get_html_end_tag(text_bytes : &[u8]) -> Option<u32> {
    static BEGIN_TAGS: &[&[u8]; 3] = &[b"pre", b"style", b"script"];
    static ST_BEGIN_TAGS: &[&[u8]; 3] = &[b"!--", b"?", b"![CDATA["];

    if scan_ch(text_bytes, b'<') == 0 {
        return None;
    }
    let text_bytes = &text_bytes[1..];

    for (beg_tag, end_tag_ix) in BEGIN_TAGS.iter().zip(0..3) {
        let tag_len = beg_tag.len();

        if text_bytes.len() < tag_len {
            // begin tags are increasing in size
            break;
        }

        if !text_bytes[..tag_len].eq_ignore_ascii_case(beg_tag) {
            continue;
        }

        // Must either be the end of the line...
        if text_bytes.len() == tag_len {
            return Some(end_tag_ix);
        }

        // ...or be followed by whitespace, newline, or '>'.
        let s = text_bytes[tag_len] as char;
        // TODO: I think this should be ASCII whitespace only
        if s.is_whitespace() || s == '>' {
            return Some(end_tag_ix);
        }
    }

    for (beg_tag, end_tag_ix) in ST_BEGIN_TAGS.iter().zip(3..6) {
        if text_bytes.starts_with(beg_tag) {
            return Some(end_tag_ix);
        }
    }

    if text_bytes.len() > 1 && text_bytes[0] == b'!' 
        && text_bytes[1] >= b'A' && text_bytes[1] <= b'Z' {
        Some(6)
    } else {
        None
    }
}

#[derive(Copy, Clone, Debug)]
struct InlineEl {
    start: TreeIndex,  // offset of tree node
    count: usize,
    c: u8,  // b'*' or b'_'
    both: bool,  // can both open and close
}

#[derive(Debug, Clone)]
struct InlineStack {
    stack: Vec<InlineEl>,
    // lower bounds for
    // _, non_both, * (mod 3), ** (mod 3), *** (mod 3)
    // for example an underscore empasis will never match
    // with any element in the stack with index smaller than lowerbounds[0]
    lower_bounds: [usize; 5],
}

impl InlineStack {
    fn new() -> InlineStack {
        InlineStack {
            stack: Vec::new(),
            lower_bounds: [0; 5],
        }
    }

    fn pop_all<'a>(&mut self, tree: &mut Tree<Item>) {
        for el in self.stack.drain(..) {
            for i in 0..el.count {
                tree[el.start + i].item.body = ItemBody::Text;
            }
        }
    }

    // both implies *, i think. because _ can never be
    // both opener and closer
    fn get_lowerbound(&self, c: u8, count: usize, both: bool) -> usize {
        if c == b'_' {
            self.lower_bounds[0]
        } else if c == b'*' {
            let mod3_lower = self.lower_bounds[2 + count % 3];
            if both {
                mod3_lower
            } else {
                min(mod3_lower, self.lower_bounds[1])
            }
        } else {
            0
        }
    }

    fn set_lowerbound(&mut self, c: u8, count: usize, both: bool, new_bound: usize) {
        if c == b'_' {
            self.lower_bounds[0] = new_bound;
        } else if c == b'*' {
            self.lower_bounds[2 + count % 3] = new_bound;
            if !both {
                self.lower_bounds[1] = new_bound;
            }
        }
    }

    fn find_match<'a>(&mut self, tree: &mut Tree<Item>, c: u8, count: usize, both: bool)
        -> Option<InlineEl>
    {
        let lowerbound = self.get_lowerbound(c, count, both);
        let res = self.stack[lowerbound..]
            .iter()
            .cloned()
            .enumerate()
            .rev()
            .find(|(_, el)| {
                el.c == c && (!both && !el.both || (count + el.count) % 3 != 0 || count % 3 == 0)
            });

        if let Some((matching_ix, matching_el)) = res {
            for i in (matching_ix + 1)..self.stack.len() {
                let el = self.stack[i];
                self.set_lowerbound(el.c, el.count, el.both, matching_ix.saturating_sub(1));
                for i in 0..el.count {
                    tree[el.start + i].item.body = ItemBody::Text;
                }
            }
            self.stack.truncate(matching_ix);
            Some(matching_el)
        } else {
            self.set_lowerbound(c, count, both, self.stack.len().saturating_sub(1));
            None
        }
    }

    fn push(&mut self, el: InlineEl) {
        self.stack.push(el)
    }
}

#[derive(Debug, Clone)]
enum RefScan<'a> {
    // label, next node index
    LinkLabel(CowStr<'a>, TreePointer),
    // contains next node index
    Collapsed(TreePointer),
    Failed,
}

fn scan_nodes_to_ix(tree: &Tree<Item>, mut node: TreePointer, ix: usize) -> TreePointer {
    while let TreePointer::Valid(node_ix) = node {
        if tree[node_ix].item.end <= ix {
            node = tree[node_ix].next;
        } else {
            break;
        }
    }
    node
}

fn scan_reference<'a, 'b>(tree: &'a Tree<Item>, text: &'b str, cur: TreePointer) -> RefScan<'b> {
    let cur_ix = match cur {
        TreePointer::Nil => return RefScan::Failed,
        TreePointer::Valid(cur_ix) => cur_ix,
    };
    let start = tree[cur_ix].item.start;
    let tail = &text.as_bytes()[start..];
    
    if tail.starts_with(b"[]") {
        let closing_node = tree[cur_ix].next.unwrap();
        RefScan::Collapsed(tree[closing_node].next)
    } else if let Some((ix, ReferenceLabel::Link(label))) = scan_link_label(&text[start..]) {
        let next_node = scan_nodes_to_ix(tree, cur, start + ix);
        RefScan::LinkLabel(label, next_node)
    } else {
        RefScan::Failed
    }
}

#[derive(Clone, Debug)]
struct LinkStackEl {
    node: TreeIndex,
    ty: LinkStackTy,
}

#[derive(PartialEq, Clone, Debug)]
enum LinkStackTy {
    Link,
    Image,
    Disabled,
}

#[derive(Clone)]
struct LinkDef<'a> {
    dest: CowStr<'a>,
    title: Option<CowStr<'a>>,
}

/// Tracks tree indices of code span delimiters of each length. It should prevent
/// quadratic scanning behaviours by providing (amortized) constant time lookups.
struct CodeDelims {
    inner: HashMap<usize, VecDeque<TreeIndex>>,
    seen_first: bool,
}

impl CodeDelims {
    fn new() -> Self {
        Self {
            inner: Default::default(),
            seen_first: false,
        }
    }

    fn insert(&mut self, count: usize, ix: TreeIndex) {
        if self.seen_first {
            self.inner.entry(count).or_insert_with(Default::default).push_back(ix);
        } else {
            // Skip the first insert, since that delimiter will always
            // be an opener and not a closer.
            self.seen_first = true;
        }        
    }

    fn is_populated(&self) -> bool {
        !self.inner.is_empty()
    }

    fn find(&mut self, open_ix: TreeIndex, count: usize) -> Option<TreeIndex> {
        while let Some(ix) = self.inner.get_mut(&count)?.pop_front() {
            if ix > open_ix {
                return Some(ix);
            }
        }
        None
    }

    fn clear(&mut self) {
        self.inner.clear();
        self.seen_first = false;
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
struct LinkIndex(usize);

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
struct CowIndex(usize);

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
struct AlignmentIndex(usize);

#[derive(Clone)]
struct Allocations<'a> {
    refdefs: HashMap<LinkLabel<'a>, LinkDef<'a>>,
    links: Vec<(LinkType, CowStr<'a>, CowStr<'a>)>,
    cows: Vec<CowStr<'a>>,
    alignments: Vec<Vec<Alignment>>,
}

impl<'a> Allocations<'a> {
    fn new() -> Self {
        Self {
            refdefs: HashMap::new(),
            links: Vec::with_capacity(128),
            cows: Vec::new(),
            alignments: Vec::new(),
        }
    }

    fn allocate_cow(&mut self, cow: CowStr<'a>) -> CowIndex {
        let ix = self.cows.len();
        self.cows.push(cow);
        CowIndex(ix)
    }

    fn allocate_link(&mut self, ty: LinkType, url: CowStr<'a>, title: CowStr<'a>) -> LinkIndex {
        let ix = self.links.len();
        self.links.push((ty, url, title));
        LinkIndex(ix)
    }

    fn allocate_alignment(&mut self, alignment: Vec<Alignment>) -> AlignmentIndex {
        let ix = self.alignments.len();
        self.alignments.push(alignment);
        AlignmentIndex(ix)
    }
}

impl<'a> Index<CowIndex> for Allocations<'a> {
    type Output = CowStr<'a>;

    fn index(&self, ix: CowIndex) -> &Self::Output {
        self.cows.index(ix.0)
    }
}

impl<'a> Index<LinkIndex> for Allocations<'a> {
    type Output = (LinkType, CowStr<'a>, CowStr<'a>);

    fn index(&self, ix: LinkIndex) -> &Self::Output {
        self.links.index(ix.0)
    }
}

impl<'a> Index<AlignmentIndex> for Allocations<'a> {
    type Output = Vec<Alignment>;

    fn index(&self, ix: AlignmentIndex) -> &Self::Output {
        self.alignments.index(ix.0)
    }
}

/// A struct containing information on the reachability of certain inline HTML
/// elements. In particular, for cdata elements (`<![CDATA[`), processing
/// elements (`<?`) and declarations (`<!DECLARATION`). The respectives usizes
/// represent the indices before which a scan will always fail and can hence
/// be skipped.
#[derive(Clone, Default)]
pub(crate) struct HtmlScanGuard {
    pub cdata: usize,
    pub processing: usize,
    pub declaration: usize,
}

#[derive(Clone)]
pub struct Parser<'a> {
    text: &'a str,
    tree: Tree<Item>,
    allocs: Allocations<'a>,
    broken_link_callback: Option<&'a Fn(&str, &str) -> Option<(String, String)>>,
    offset: usize,
    html_scan_guard: HtmlScanGuard,

    // used by inline passes. store them here for reuse
    inline_stack: InlineStack,
    link_stack: Vec<LinkStackEl>,
}

impl<'a> Parser<'a> {
    pub fn new(text: &'a str) -> Parser<'a> {
        Parser::new_ext(text, Options::empty())
    }

    pub fn new_ext(text: &'a str, options: Options) -> Parser<'a> {
        Parser::new_with_broken_link_callback(text, options, None)
    }

    /// In case the parser encounters any potential links that have a broken
    /// reference (e.g `[foo]` when there is no `[foo]: ` entry at the bottom)
    /// the provided callback will be called with the reference name,
    /// and the returned pair will be used as the link name and title if not
    /// None.
    pub fn new_with_broken_link_callback(
        text: &'a str,
        options: Options,
        broken_link_callback: Option<&'a Fn(&str, &str) -> Option<(String, String)>>
    ) -> Parser<'a> {
        let first_pass = FirstPass::new(text, options);
        let (mut tree, allocs) = first_pass.run();
        tree.reset();
        let inline_stack = InlineStack::new();
        let link_stack = Vec::new();
        let html_scan_guard = Default::default();
        Parser {
            text, tree, allocs, broken_link_callback,
            offset: 0, inline_stack, link_stack, html_scan_guard
        }
    }

    pub fn get_offset(&self) -> usize {
        self.offset
    }

    /// Handle inline markup.
    ///
    /// When the parser encounters any item indicating potential inline markup, all
    /// inline markup passes are run on the remainder of the chain.
    ///
    /// Note: there's some potential for optimization here, but that's future work.
    fn handle_inline(&mut self) {
        self.handle_inline_pass1();
        self.handle_emphasis();
    }

    /// Handle inline HTML, code spans, and links.
    ///
    /// This function handles both inline HTML and code spans, because they have
    /// the same precedence. It also handles links, even though they have lower
    /// precedence, because the URL of links must not be processed.
    fn handle_inline_pass1(&mut self) {
        let mut code_delims = CodeDelims::new();
        let mut cur = self.tree.cur();
        let mut prev = TreePointer::Nil;

        while let TreePointer::Valid(mut cur_ix) = cur {
            match self.tree[cur_ix].item.body {
                ItemBody::MaybeHtml => {
                    let next = self.tree[cur_ix].next;
                    let autolink = if let TreePointer::Valid(next_ix) = next {
                        scan_autolink(self.text, self.tree[next_ix].item.start)
                    } else {
                        None
                    };

                    if let Some((ix, uri, link_type)) = autolink {
                        let node = scan_nodes_to_ix(&self.tree, next, ix);
                        let text_node = self.tree.create_node(Item {
                            start: self.tree[cur_ix].item.start + 1,
                            end: ix - 1,
                            body: ItemBody::Text,
                        });
                        let link_ix = self.allocs.allocate_link(link_type, uri, "".into());
                        self.tree[cur_ix].item.body = ItemBody::Link(link_ix);
                        self.tree[cur_ix].item.end = ix;
                        self.tree[cur_ix].next = node;
                        self.tree[cur_ix].child = TreePointer::Valid(text_node);
                        cur = node;
                        if let TreePointer::Valid(node_ix) = cur {
                            self.tree[node_ix].item.start = ix;
                        }
                        continue;
                    } else {
                        let inline_html = if let TreePointer::Valid(next_ix) = next {
                            self.tree.peek_up()
                                .map(|parent_ix| self.tree[parent_ix].item.end)
                                .and_then(|end_offset| {
                                    let bytes = &self.text.as_bytes()[..end_offset];
                                    scan_inline_html(bytes, self.tree[next_ix].item.start, &mut self.html_scan_guard)
                                })
                        } else {
                            None
                        };
                        if let Some(ix) = inline_html {
                            let node = scan_nodes_to_ix(&self.tree, next, ix);
                            // TODO: this logic isn't right if the replaced chain has
                            // tricky stuff (skipped containers, replaced nulls).
                            self.tree[cur_ix].item.body = ItemBody::InlineHtml;
                            self.tree[cur_ix].item.end = ix;
                            self.tree[cur_ix].next = node;
                            cur = node;
                            if let TreePointer::Valid(node_ix) = cur {
                                self.tree[node_ix].item.start = ix;
                            }
                            continue;
                        }
                    }
                    self.tree[cur_ix].item.body = ItemBody::Text;
                }
                ItemBody::MaybeCode(mut search_count) => {
                    if let TreePointer::Valid(prev_ix) = prev {
                        if self.tree[prev_ix].item.body == ItemBody::Backslash {
                            search_count -= 1;
                        }
                    }

                    if code_delims.is_populated() {
                        // we have previously scanned all codeblock delimiters,
                        // so we can reuse that work
                        if let Some(scan_ix) = code_delims.find(cur_ix, search_count) {
                            self.make_code_span(cur_ix, scan_ix);
                        } else {
                            self.tree[cur_ix].item.body = ItemBody::Text;
                        }
                    } else {
                        // we haven't previously scanned all codeblock delimiters,
                        // so walk the AST
                        let mut scan = if search_count > 0 { self.tree[cur_ix].next } else { TreePointer::Nil };
                        while let TreePointer::Valid(scan_ix) = scan {
                            if let ItemBody::MaybeCode(delim_count) = self.tree[scan_ix].item.body {
                                if search_count == delim_count {
                                    self.make_code_span(cur_ix, scan_ix);
                                    code_delims.clear();
                                    break;
                                } else {
                                    code_delims.insert(delim_count, scan_ix);
                                }
                            }
                            scan = self.tree[scan_ix].next;
                        }
                        if scan == TreePointer::Nil {
                            self.tree[cur_ix].item.body = ItemBody::Text;
                        }
                    }
                }
                ItemBody::MaybeLinkOpen => {
                    self.tree[cur_ix].item.body = ItemBody::Text;
                    self.link_stack.push( LinkStackEl { node: cur_ix, ty: LinkStackTy::Link });
                }
                ItemBody::MaybeImage => {
                    self.tree[cur_ix].item.body = ItemBody::Text;
                    self.link_stack.push( LinkStackEl { node: cur_ix, ty: LinkStackTy::Image });
                }
                ItemBody::MaybeLinkClose => {
                    if let Some(tos) = self.link_stack.pop() {
                        if tos.ty == LinkStackTy::Disabled {
                            self.tree[cur_ix].item.body = ItemBody::Text;
                            continue;
                        }
                        let next = self.tree[cur_ix].next;
                        let link_details = if let TreePointer::Valid(next_ix) = next {
                            scan_inline_link(self.text, self.tree[next_ix].item.start)
                        } else {
                            None
                        };

                        if let Some((next_ix, url, title)) = link_details {
                            let next_node = scan_nodes_to_ix(&self.tree, next, next_ix);
                            if let TreePointer::Valid(prev_ix) = prev {
                                self.tree[prev_ix].next = TreePointer::Nil;
                            }                            
                            cur = TreePointer::Valid(tos.node);
                            cur_ix = tos.node;
                            let link_ix = self.allocs.allocate_link(LinkType::Inline, url, title);
                            self.tree[cur_ix].item.body = if tos.ty == LinkStackTy::Image {
                                ItemBody::Image(link_ix)
                            } else {
                                ItemBody::Link(link_ix)
                            };
                            self.tree[cur_ix].child = self.tree[cur_ix].next;
                            self.tree[cur_ix].next = next_node;
                            if let TreePointer::Valid(next_node_ix) = next_node {
                                self.tree[next_node_ix].item.start = next_ix;
                            }

                            if tos.ty == LinkStackTy::Link {
                                for el in &mut self.link_stack {
                                    if el.ty == LinkStackTy::Link {
                                        el.ty = LinkStackTy::Disabled;
                                    }
                                }
                            }
                        } else {
                            // ok, so its not an inline link. maybe it is a reference
                            // to a defined link?
                            let scan_result = scan_reference(&self.tree, &self.text, next);
                            let label_node = self.tree[tos.node].next;
                            let node_after_link = match scan_result {
                                RefScan::LinkLabel(_, next_node) => next_node,
                                RefScan::Collapsed(next_node) => next_node,
                                RefScan::Failed => next,
                            };
                            let link_type = match &scan_result {
                                RefScan::LinkLabel(..) => LinkType::Reference,
                                RefScan::Collapsed(..) => LinkType::Collapsed,
                                RefScan::Failed => LinkType::Shortcut,
                            };
                            let label: Option<ReferenceLabel<'a>> = match scan_result {
                                RefScan::LinkLabel(l, ..) => Some(ReferenceLabel::Link(l)),
                                RefScan::Collapsed(..) | RefScan::Failed => {
                                    // No label? maybe it is a shortcut reference
                                    let start = self.tree[tos.node].item.end - 1;
                                    let end = self.tree[cur_ix].item.end;
                                    let search_text = &self.text[start..end];

                                    scan_link_label(search_text).map(|(_ix, label)| label)
                                }
                            };

                            // see if it's a footnote reference
                            if let Some(ReferenceLabel::Footnote(l)) = label {
                                self.tree[tos.node].next = node_after_link;
                                self.tree[tos.node].child = TreePointer::Nil;
                                self.tree[tos.node].item.body = ItemBody::FootnoteReference(self.allocs.allocate_cow(l));
                                prev = TreePointer::Valid(tos.node);
                                cur = node_after_link;
                                self.link_stack.clear();
                                continue;
                            } else if let Some(ReferenceLabel::Link(link_label)) = label {
                                let type_url_title = if let Some(matching_def) = self.allocs.refdefs.get(&UniCase::new(link_label.as_ref().into())) {
                                    // found a matching definition!
                                    let title = matching_def.title.as_ref().cloned().unwrap_or("".into());
                                    let url = matching_def.dest.clone();
                                    Some((link_type, url, title))
                                } else if let Some(callback) = self.broken_link_callback {
                                    // looked for matching definition, but didn't find it. try to fix
                                    // link with callback, if it is defined
                                    if let Some((url, title)) = callback(link_label.as_ref(), link_label.as_ref()) {
                                        Some((link_type.to_unknown(), url.into(), title.into()))
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                };

                                if let Some((def_link_type, url, title)) = type_url_title {
                                    let link_ix = self.allocs.allocate_link(def_link_type, url, title);
                                    self.tree[tos.node].item.body = if tos.ty == LinkStackTy::Image {
                                        ItemBody::Image(link_ix)
                                    } else {
                                        ItemBody::Link(link_ix)
                                    };

                                    // lets do some tree surgery to add the link to the tree
                                    // 1st: skip the label node and close node
                                    self.tree[tos.node].next = node_after_link;

                                    // then, add the label node as a child to the link node
                                    self.tree[tos.node].child = label_node;

                                    // finally: disconnect list of children
                                    if let TreePointer::Valid(prev_ix) = prev {
                                        self.tree[prev_ix].next = TreePointer::Nil;
                                    }                                

                                    // set up cur so next node will be node_after_link
                                    cur = TreePointer::Valid(tos.node);
                                    cur_ix = tos.node;

                                    if tos.ty == LinkStackTy::Link {
                                        for el in &mut self.link_stack {
                                            if el.ty == LinkStackTy::Link {
                                                el.ty = LinkStackTy::Disabled;
                                            }
                                        }
                                    }
                                } else {
                                    self.tree[cur_ix].item.body = ItemBody::Text;
                                }
                            } else {
                                self.tree[cur_ix].item.body = ItemBody::Text;
                            }
                        }
                    } else {
                        self.tree[cur_ix].item.body = ItemBody::Text;
                    }
                }
                _ => (),
            }
            prev = cur;
            cur = self.tree[cur_ix].next;
        }
    }

    fn handle_emphasis(&mut self) {
        let mut prev = TreePointer::Nil;
        let mut prev_ix: TreeIndex;
        let mut cur = self.tree.cur();
        while let TreePointer::Valid(mut cur_ix) = cur {
            if let ItemBody::MaybeEmphasis(mut count, can_open, can_close) = self.tree[cur_ix].item.body {
                let c = self.text.as_bytes()[self.tree[cur_ix].item.start];
                let both = can_open && can_close;
                if can_close {
                    while let Some(el) = self.inline_stack.find_match(&mut self.tree, c, count, both) {
                        // have a match!
                        if let TreePointer::Valid(prev_ix) = prev {
                            self.tree[prev_ix].next = TreePointer::Nil;
                        }                        
                        let match_count = min(count, el.count);
                        // start, end are tree node indices
                        let mut end = cur_ix - 1;
                        let mut start = el.start + el.count;

                        // work from the inside out
                        while start > el.start + el.count - match_count {
                            let (inc, ty) = if c == b'~' {
                                (2, ItemBody::Strikethrough)
                            } else if start > el.start + el.count - match_count + 1 {
                                (2, ItemBody::Strong)
                            } else {
                                (1, ItemBody::Emphasis)
                            };

                            let root = start - inc;
                            end = end + inc;
                            self.tree[root].item.body = ty;
                            self.tree[root].item.end = self.tree[end].item.end;
                            self.tree[root].child = TreePointer::Valid(start);
                            self.tree[root].next = TreePointer::Nil;
                            start = root;
                        }

                        // set next for top most emph level
                        prev_ix = el.start + el.count - match_count;
                        prev = TreePointer::Valid(prev_ix);
                        cur = self.tree[cur_ix + match_count - 1].next;
                        self.tree[prev_ix].next = cur;

                        if el.count > match_count {
                            self.inline_stack.push(InlineEl {
                                start: el.start,
                                count: el.count - match_count,
                                c: el.c,
                                both,
                            })
                        }
                        count -= match_count;
                        if count > 0 {
                            cur_ix = cur.unwrap();
                        } else {
                            break;
                        }
                    }
                }
                if count > 0 {
                    if can_open {
                        self.inline_stack.push(InlineEl {
                            start: cur_ix,
                            count,
                            c,
                            both,
                        });
                    } else {
                        for i in 0..count {
                            self.tree[cur_ix + i].item.body = ItemBody::Text;
                        }
                    }
                    prev_ix = cur_ix + count - 1;
                    prev = TreePointer::Valid(prev_ix);
                    cur = self.tree[prev_ix].next;
                }
            } else {
                prev = cur;
                cur = self.tree[cur_ix].next;
            }
        }
        self.inline_stack.pop_all(&mut self.tree);
    }

    /// Make a code span.
    ///
    /// Both `open` and `close` are matching MaybeCode items.
    fn make_code_span(&mut self, open: TreeIndex, close: TreeIndex) {
        let first_ix = open + 1;
        let last_ix = close - 1;
        let bytes = self.text.as_bytes();
        let mut span_start = self.tree[first_ix].item.start;
        let mut span_end = self.tree[close].item.start;
        let mut buf: Option<String> = None;

        // detect all-space sequences, since they are kept as-is as of commonmark 0.29
        if ! bytes[span_start..span_end].iter().all(|&b| b == b' ') {
            let opening = match bytes[span_start]   { b' ' | b'\r' | b'\n' => true, _ => false };
            let closing = match bytes[span_end - 1] { b' ' | b'\r' | b'\n' => true, _ => false };
            let drop_enclosing_whitespace = opening && closing;

            if drop_enclosing_whitespace {
                span_start += 1;
                if span_start < span_end  {
                    span_end -= 1;
                }
            }

            let mut ix = first_ix;

            while ix < close {
                match self.tree[ix].item.body {
                    ItemBody::HardBreak | ItemBody::SoftBreak => {
                        if drop_enclosing_whitespace &&
                            (ix == first_ix && bytes[self.tree[ix].item.start] != b'\\') ||
                            (ix == last_ix && last_ix > first_ix) {
                            // just ignore it
                        } else {
                            let end = bytes[self.tree[ix].item.start..]
                                .iter()
                                .position(|&b| b == b'\r' || b == b'\n')
                                .unwrap()
                                + self.tree[ix].item.start;
                            if let Some(ref mut buf) = buf {
                                buf.push_str(&self.text[self.tree[ix].item.start..end]);
                                buf.push(' ');
                            } else {
                                let mut new_buf = String::with_capacity(span_end - span_start);
                                new_buf.push_str(&self.text[span_start..end]);
                                new_buf.push(' ');
                                buf = Some(new_buf);
                            }
                        }
                    }
                    _ => {
                        if let Some(ref mut buf) = buf {
                            let end = if ix == last_ix {
                                span_end
                            } else {
                                self.tree[ix].item.end
                            };
                            buf.push_str(&self.text[self.tree[ix].item.start..end]);
                        }
                    }
                }
                ix = ix + 1;
            }
        }

        let cow = if let Some(buf) = buf {
            buf.into()
        } else {
            self.text[span_start..span_end].into()
        };
        self.tree[open].item.body = ItemBody::Code(self.allocs.allocate_cow(cow));
        self.tree[open].item.end = self.tree[close].item.end;
        self.tree[open].next = self.tree[close].next;
        self.tree[open].child = TreePointer::Nil;
    }

    pub fn into_offset_iter(self) -> OffsetIter<'a> {
        OffsetIter {
            inner: self,
        }
    }
}

pub(crate) enum LoopInstruction<T> {
    /// Continue looking for more special bytes, but skip next few bytes.
    ContinueAndSkip(usize),
    /// Break looping immediately, returning with the given index and value.
    BreakAtWith(usize, T)
}

/// This function walks the byte slices from the given index and
/// calls the callback function on all bytes (and their indices) that are in the following set:
/// `` ` ``, `\`, `&`, `*`, `_`, `~`, `!`, `<`, `[`, `]`, `|`, `\r`, `\n`
/// It may also call the callback on other bytes, but it is not guaranteed.
/// Whenever `callback(ix, byte)` returns a `ContinueAndSkip(n)` value, the callback
/// will not be called with an index that is less than `ix + n + 1`.
/// When the callback returns a `BreakAtWith(end_ix, opt+val)`, no more callbacks will be
/// called and the function returns immediately with the return value `(end_ix, opt_val)`.
/// If `BreakAtWith(..)` is never returned, this function will return the first
/// index that is outside the byteslice bound and a `None` value.
fn iterate_special_bytes<F, T>(bytes: &[u8], ix: usize, callback: F) -> (usize, Option<T>)
    where F: FnMut(usize, u8) -> LoopInstruction<Option<T>> 
{
    #[cfg(all(target_arch = "x86_64", feature="simd"))]
    { crate::simd::iterate_special_bytes(bytes, ix, callback) }
    #[cfg(not(all(target_arch = "x86_64", feature="simd")))]
    { scalar_iterate_special_bytes(bytes, ix, callback) }
}

pub(crate) fn scalar_iterate_special_bytes<F, T>(bytes: &[u8], mut ix: usize, mut callback: F) -> (usize, Option<T>)
    where F: FnMut(usize, u8) -> LoopInstruction<Option<T>> 
{
    while ix < bytes.len() {
        match callback(ix, bytes[ix]) {
            LoopInstruction::ContinueAndSkip(skip) => {
                ix += skip + 1;
            }
            LoopInstruction::BreakAtWith(ix, val) => {
                return (ix, val);
            }
        }
    }

    (ix, None)
}

pub struct OffsetIter<'a> {
    inner: Parser<'a>,
}

impl<'a> Iterator for OffsetIter<'a> {
    type Item = (Event<'a>, Range<usize>);

    fn next(&mut self) -> Option<Self::Item> {
        match self.inner.tree.cur() {
            TreePointer::Nil => {
                let ix = self.inner.tree.pop()?;
                let tag = item_to_tag(&self.inner.tree[ix].item, &self.inner.allocs).unwrap();
                self.inner.tree.next_sibling(ix);
                Some((Event::End(tag), self.inner.tree[ix].item.start..self.inner.tree[ix].item.end))
            }
            TreePointer::Valid(mut cur_ix) => {
                if let ItemBody::Backslash = self.inner.tree[cur_ix].item.body {
                    if let TreePointer::Valid(next) = self.inner.tree.next_sibling(cur_ix) {
                        cur_ix = next;
                    }
                }
                if self.inner.tree[cur_ix].item.body.is_inline() {
                    self.inner.handle_inline();
                }

                if let Some(tag) = item_to_tag(&self.inner.tree[cur_ix].item, &self.inner.allocs) {
                    self.inner.tree.push();                
                    Some((Event::Start(tag), self.inner.tree[cur_ix].item.start..self.inner.tree[cur_ix].item.end))
                } else {
                    self.inner.tree.next_sibling(cur_ix);
                    let item = &self.inner.tree[cur_ix].item;
                    Some((item_to_event(item, self.inner.text, &self.inner.allocs), item.start..item.end))
                }
            }
        }
    }
}

fn item_to_tag<'a>(item: &Item, allocs: &Allocations<'a>) -> Option<Tag<'a>> {
    match item.body {
        ItemBody::Paragraph => Some(Tag::Paragraph),
        ItemBody::Emphasis => Some(Tag::Emphasis),
        ItemBody::Strong => Some(Tag::Strong),
        ItemBody::Strikethrough => Some(Tag::Strikethrough),
        ItemBody::Link(link_ix) => {
            let &(ref link_type, ref url, ref title) = allocs.index(link_ix);
            Some(Tag::Link(*link_type, url.clone(), title.clone()))
        }
        ItemBody::Image(link_ix) => {
            let &(ref link_type, ref url, ref title) = allocs.index(link_ix);
            Some(Tag::Image(*link_type, url.clone(), title.clone()))
        }
        ItemBody::Rule => Some(Tag::Rule),
        ItemBody::Header(level) => Some(Tag::Header(level)),
        ItemBody::FencedCodeBlock(cow_ix) =>
            Some(Tag::CodeBlock(allocs[cow_ix].clone())),
        ItemBody::IndentCodeBlock => Some(Tag::CodeBlock("".into())),
        ItemBody::BlockQuote => Some(Tag::BlockQuote),
        ItemBody::List(_, c, listitem_start) => {
            if c == b'.' || c == b')' {
                Some(Tag::List(Some(listitem_start)))
            } else {
                Some(Tag::List(None))
            }
        }
        ItemBody::ListItem(_) => Some(Tag::Item),
        ItemBody::HtmlBlock(_) => Some(Tag::HtmlBlock),
        ItemBody::TableHead => Some(Tag::TableHead),
        ItemBody::TableCell => Some(Tag::TableCell),
        ItemBody::TableRow => Some(Tag::TableRow),
        ItemBody::Table(alignment_ix) => {
            Some(Tag::Table(allocs[alignment_ix].clone()))
        }
        ItemBody::FootnoteDefinition(cow_ix) =>
            Some(Tag::FootnoteDefinition(allocs[cow_ix].clone())),
        _ => None,
    }
}

// leaf items only
fn item_to_event<'a>(item: &Item, text: &'a str, allocs: &Allocations<'a>) -> Event<'a> {
    match item.body {
        ItemBody::Text => {
            Event::Text(text[item.start..item.end].into())
        }
        ItemBody::Code(cow_ix) => {
            Event::Code(allocs[cow_ix].clone())
        }
        ItemBody::SynthesizeText(cow_ix) => {
            Event::Text(allocs[cow_ix].clone())
        }
        ItemBody::Html => {
            Event::Html(text[item.start..item.end].into())
        }
        ItemBody::InlineHtml => {
            Event::InlineHtml(text[item.start..item.end].into())
        }
        ItemBody::SoftBreak => Event::SoftBreak,
        ItemBody::HardBreak => Event::HardBreak,
        ItemBody::FootnoteReference(cow_ix) => {
            Event::FootnoteReference(allocs[cow_ix].clone())
        }
        ItemBody::TaskListMarker(checked) => Event::TaskListMarker(checked),
        _ => panic!("unexpected item body {:?}", item.body)
    }
}

// https://english.stackexchange.com/a/285573
fn surgerize_tight_list<'a>(tree : &mut Tree<Item>, list_ix: TreeIndex) {
    let mut list_item = tree[list_ix].child;
    while let TreePointer::Valid(listitem_ix) = list_item {
        // first child is special, controls how we repoint list_item.child
        let list_item_firstborn = tree[listitem_ix].child;

        // Check that list item has children - this is not necessarily the case!
        if let TreePointer::Valid(firstborn_ix) = list_item_firstborn {
            if let ItemBody::Paragraph = tree[firstborn_ix].item.body {
                // paragraphs should always have children
                tree[listitem_ix].child = tree[firstborn_ix].child;
            }

            let mut list_item_child = TreePointer::Valid(firstborn_ix);
            let mut node_to_repoint = TreePointer::Nil;
            while let TreePointer::Valid(child_ix) = list_item_child {
                // surgerize paragraphs
                let repoint_ix = if let ItemBody::Paragraph = tree[child_ix].item.body {
                    // no empty paragraphs!
                    let child_firstborn = tree[child_ix].child.unwrap();
                    if let TreePointer::Valid(repoint_ix) = node_to_repoint {
                        tree[repoint_ix].next = TreePointer::Valid(child_firstborn);
                    }
                    let mut child_lastborn = child_firstborn;
                    while let TreePointer::Valid(lastborn_next_ix) = tree[child_lastborn].next {
                        child_lastborn = lastborn_next_ix;
                    }
                    child_lastborn
                } else {
                    child_ix
                };

                node_to_repoint = TreePointer::Valid(repoint_ix);
                tree[repoint_ix].next = tree[child_ix].next;
                list_item_child = tree[child_ix].next;
            }
        }

        list_item = tree[listitem_ix].next;
    }
}

impl<'a> Iterator for Parser<'a> {
    type Item = Event<'a>;

    fn next(&mut self) -> Option<Event<'a>> {
        match self.tree.cur() {
            TreePointer::Nil => {
                let ix = self.tree.pop()?;
                let tag = item_to_tag(&self.tree[ix].item, &self.allocs).unwrap();
                self.offset = self.tree[ix].item.end;
                self.tree.next_sibling(ix);
                Some(Event::End(tag))
            }
            TreePointer::Valid(mut cur_ix) => {
                if let ItemBody::Backslash = self.tree[cur_ix].item.body {
                    if let TreePointer::Valid(next) = self.tree.next_sibling(cur_ix) {
                        cur_ix = next;
                    }
                }
                if self.tree[cur_ix].item.body.is_inline() {
                    self.handle_inline();
                }

                if let Some(tag) = item_to_tag(&self.tree[cur_ix].item, &self.allocs) {
                    self.offset = if let TreePointer::Valid(child_ix) = self.tree[cur_ix].child {
                        self.tree[child_ix].item.start
                    } else {
                        self.tree[cur_ix].item.end
                    };
                    self.tree.push();                
                    Some(Event::Start(tag))
                } else {
                    self.tree.next_sibling(cur_ix);
                    let item = &self.tree[cur_ix].item;
                    self.offset = item.end;
                    Some(item_to_event(item, self.text, &self.allocs))
                }
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::tree::Node;

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn node_size() {
        let node_size = std::mem::size_of::<Node<Item>>();
        assert_eq!(48, node_size);
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn body_size() {
        let body_size = std::mem::size_of::<ItemBody>();
        assert_eq!(16, body_size);
    }

    #[test]
    fn single_open_fish_bracket() {
        // dont crash
        assert_eq!(3, Parser::new("<").count());
    }

    #[test]
    fn offset_iter() {
        let event_offsets: Vec<_> = Parser::new("*hello* world")
            .into_offset_iter()
            .map(|(_ev, range)| range)
            .collect();
        let expected_offsets = vec![
            (0..13),
                (0..7),
                    (1..6),
                (0..7),
                (7..13),
            (0..13)
        ];
        assert_eq!(expected_offsets, event_offsets);
    }

    #[test]
    fn link_def_at_eof() {
        let test_str = "[My site][world]\n\n[world]: https://vincentprouillet.com";
        let expected = "<p><a href=\"https://vincentprouillet.com\">My site</a></p>\n";

        let mut buf = String::new();
        crate::html::push_html(&mut buf, Parser::new(test_str));
        assert_eq!(expected, buf);
    }

    #[test]
    fn simple_broken_link_callback() {
        let test_str = "This is a link w/o def: [hello][world]";
        let parser = Parser::new_with_broken_link_callback(test_str, Options::empty(), Some(&|norm, raw| {
            assert_eq!("world", raw);
            assert_eq!("world", norm);
            Some(("YOLO".to_owned(), "SWAG".to_owned()))
        }));
        let mut link_tag_count = 0;
        for (typ, url, title) in parser.filter_map(|event| match event {
            Event::Start(tag) | Event::End(tag) => match tag {
                Tag::Link(typ, url, title) => Some((typ, url, title)),
                _ => None,
            }
            _ => None,
        }) {
            link_tag_count += 1;
            assert_eq!(typ, LinkType::ReferenceUnknown);
            assert_eq!(url.as_ref(), "YOLO");
            assert_eq!(title.as_ref(), "SWAG");
        }
        assert!(link_tag_count > 0);
    }
}
