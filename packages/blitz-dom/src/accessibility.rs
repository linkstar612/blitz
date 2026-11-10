use crate::{BaseDocument, ElementData, Node as BlitzDomNode, local_name};
use accesskit::{Node as AccessKitNode, NodeId, Role, Toggled, TreeId, TreeInfo, TreeUpdate};
use icu_segmenter::{
    GraphemeClusterSegmenter, GraphemeClusterSegmenterBorrowed, WordSegmenter,
    WordSegmenterBorrowed, options::WordBreakInvariantOptions,
};
use style::properties::longhands::visibility;

const GRAPHEMES: GraphemeClusterSegmenterBorrowed<'static> = GraphemeClusterSegmenter::new();
const WORDS: WordSegmenterBorrowed<'static> =
    WordSegmenter::new_for_non_complex_scripts(WordBreakInvariantOptions::default());

/// polyvox R-ONB.1: a text run holds at most this many characters, because
/// AccessKit stores word starts as `u8` character indices.
const MAX_RUN_CHARS: usize = 255;

/// polyvox R-ONB.1: ids for the second and later runs of a long or
/// multi-line text node. Blitz node ids are slab indices, far below this, and
/// the window takes `u64::MAX`. blitz-shell ignores action requests, so no
/// id here has to map back to a DOM node.
const EXTRA_RUN_ID: u64 = 1 << 63;

/// One AccessKit text run cut from a DOM text node.
struct TextRunPiece {
    value: String,
    character_lengths: Vec<u8>,
    word_starts: Vec<u8>,
}

/// polyvox R-ONB.1: a DOM text node as AccessKit text runs.
///
/// accesskit_consumer (0.39) takes a run's length from `character_lengths`
/// alone, so a run without them is zero characters long: a position at its
/// start is also at its end. `Position::backward_to_paragraph_start` then
/// steps from the start of the next run back onto the same position forever,
/// inside the platform adapter's read lock on the tree, so the UI thread's
/// next tree update waits forever and the window stops answering. And
/// `backward_to_character_start` subtracts one from index 0, which panics
/// with overflow checks and leaves the run without them.
///
/// So every run carries what `Node::character_lengths` and
/// `Node::word_starts` ask for: one length per grapheme cluster in UTF-8
/// bytes, summing to the value's length (a cluster longer than 255 bytes is
/// cut at char boundaries, since the lengths are `u8`), and the start of each
/// word, where a word is an ICU word segment with the whitespace after it.
/// A run ends after a hard line break, as AccessKit asks, and holds at most
/// [`MAX_RUN_CHARS`] characters, cut at a word start when one is in reach.
/// Each run labels its parent, and the consumer joins labels with a space,
/// so a cut adds whitespace to a name and never drops text. An empty text
/// node gives no run at all: a run of zero characters is the case above.
fn text_run_pieces(text: &str) -> Vec<TextRunPiece> {
    // Characters as (byte offset, length).
    let mut chars: Vec<(usize, u8)> = Vec::new();
    let mut prev = 0;
    for end in GRAPHEMES.segment_str(text).skip(1) {
        let mut at = prev;
        while end - at > usize::from(u8::MAX) {
            let mut cut = at + usize::from(u8::MAX);
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            chars.push((at, (cut - at) as u8));
            at = cut;
        }
        chars.push((at, (end - at) as u8));
        prev = end;
    }
    let is_break = |i: usize| {
        let (at, len) = chars[i];
        matches!(&text[at..at + usize::from(len)], "\n" | "\r\n" | "\r")
    };

    // Word starts as character indices, sorted.
    let mut word_starts = vec![0usize];
    let mut next = 0;
    let mut breaks = WORDS.segment_str(text).peekable();
    while let Some(seg_start) = breaks.next() {
        let Some(&seg_end) = breaks.peek() else { break };
        while next < chars.len() && chars[next].0 < seg_start {
            next += 1;
        }
        if next == 0 || next >= chars.len() || chars[next].0 != seg_start {
            continue;
        }
        if !text[seg_start..seg_end].chars().all(char::is_whitespace) {
            word_starts.push(next);
        }
    }

    let mut pieces = Vec::new();
    let mut first = 0;
    while first < chars.len() {
        let limit = (first + MAX_RUN_CHARS).min(chars.len());
        let mut last = (first..limit).find(|&i| is_break(i)).map_or(limit, |i| i + 1);
        if last == limit && limit < chars.len() {
            if let Some(&w) = word_starts.iter().rev().find(|&&w| w > first && w <= limit) {
                last = w;
            }
        }
        let byte_end = chars.get(last).map_or(text.len(), |c| c.0);
        let starts_paragraph = first == 0 || is_break(first - 1);
        let mut run_word_starts: Vec<u8> = word_starts
            .iter()
            .filter(|&&w| w >= first && w < last)
            .map(|&w| (w - first) as u8)
            .collect();
        if starts_paragraph && run_word_starts.first() != Some(&0) {
            run_word_starts.insert(0, 0);
        }
        pieces.push(TextRunPiece {
            value: text[chars[first].0..byte_end].to_owned(),
            character_lengths: chars[first..last].iter().map(|c| c.1).collect(),
            word_starts: run_word_starts,
        });
        first = last;
    }
    pieces
}

impl BaseDocument {
    pub fn build_accessibility_tree(&self) -> TreeUpdate {
        let mut nodes = std::collections::HashMap::new();
        let mut window = AccessKitNode::new(Role::Window);
        let mut hidden_nodes = std::collections::HashSet::new();
        let mut extra_runs = Vec::new();

        self.visit(|node_id, node| {
            if node.is_hidden_from_accessibility_tree()
                || node
                    .parent
                    .map(|p| hidden_nodes.contains(&p))
                    .unwrap_or(false)
            {
                hidden_nodes.insert(node_id);
                return;
            }
            let parent = node
                .parent
                .and_then(|parent_id| nodes.get_mut(&parent_id))
                .map(|(_, parent)| parent)
                .unwrap_or(&mut window);
            if node.is_text_node() {
                // polyvox R-ONB.1: see `text_run_pieces`.
                let text = node.text_content();
                let mut runs = Vec::new();
                for (i, piece) in text_run_pieces(&text).into_iter().enumerate() {
                    let id = if i == 0 {
                        NodeId(node.id.as_u64())
                    } else {
                        NodeId(EXTRA_RUN_ID | (node.id.as_u64() << 20) | i as u64)
                    };
                    let mut run = AccessKitNode::new(Role::TextRun);
                    run.set_value(piece.value);
                    run.set_character_lengths(piece.character_lengths);
                    run.set_word_starts(piece.word_starts);
                    parent.push_child(id);
                    parent.push_labelled_by(id);
                    runs.push((id, run));
                }
                let mut runs = runs.into_iter();
                if let Some(first) = runs.next() {
                    nodes.insert(node_id, first);
                }
                extra_runs.extend(runs);
                return;
            }
            let (id, builder) = self.build_accessibility_node(node, parent);

            nodes.insert(node_id, (id, builder));
        });

        let mut nodes: Vec<_> = nodes
            .into_iter()
            .map(|(_, (id, node))| (id, node))
            .collect();
        nodes.extend(extra_runs);
        nodes.push((NodeId(u64::MAX), window));

        let tree = TreeInfo::new(NodeId(u64::MAX));
        TreeUpdate {
            tree_id: TreeId::ROOT,
            nodes,
            tree: Some(tree),
            focus: NodeId(self.focus_node_id.map(|id| id.as_u64()).unwrap_or(u64::MAX)),
        }
    }

    fn build_accessibility_node(
        &self,
        node: &BlitzDomNode,
        parent: &mut AccessKitNode,
    ) -> (NodeId, AccessKitNode) {
        let id = NodeId(node.id.as_u64());

        let mut builder = AccessKitNode::default();
        if node.parent.is_none() {
            builder.set_role(Role::Window)
        } else if let Some(element_data) = node.element_data() {
            let name = element_data.name.local.to_string();
            let role_attr = element_data.attr(local_name!("role"));

            // TODO: The roles of elements with strong native semantics cannot be overridden; see
            // https://www.w3.org/TR/wai-aria-1.2/#host_general_conflict.
            let role = role_attr
                .and_then(role_from_name)
                .or_else(|| role_from_element_data(element_data))
                .unwrap_or(Role::Unknown);

            builder.set_role(role);
            builder.set_html_tag(name);

            // https://www.w3.org/TR/wai-aria-1.2/#tree_exclusion
            if element_data.attr(local_name!("aria-hidden")) == Some("true") {
                builder.set_hidden();
            }

            set_name_and_states(&mut builder, element_data);
        }

        parent.push_child(id);

        (id, builder)
    }
}

impl BlitzDomNode {
    // https://www.w3.org/TR/wai-aria-1.2/#tree_exclusion
    fn is_hidden_from_accessibility_tree(&self) -> bool {
        self.try_stylo_element_data()
            .as_ref()
            .and_then(|s| s.get())
            .map(|s| {
                s.styles.is_display_none()
                    || s.styles.primary().clone_visibility()
                        == visibility::computed_value::T::Hidden
            })
            .unwrap_or(false)
    }
}

/// polyvox R-OMS.40: the accessible name and the checked, pressed, expanded,
/// disabled and value states, from ARIA first and the native control second,
/// so a screen reader can name a kit control (a `div` with `role`) and read
/// its state. <https://www.w3.org/TR/html-aam-1.0/>
fn set_name_and_states(builder: &mut AccessKitNode, el: &ElementData) {
    let attr = |name| el.attr(name).map(str::trim).filter(|v| !v.is_empty());

    // Name: `aria-label`, then `title` (accname step 2I, the tooltip).
    if let Some(label) = attr(local_name!("aria-label")).or_else(|| attr(local_name!("title"))) {
        builder.set_label(label);
    }

    let tristate = |value: &str| match value {
        "true" => Some(Toggled::True),
        "false" => Some(Toggled::False),
        "mixed" => Some(Toggled::Mixed),
        _ => None,
    };
    let toggled = attr(local_name!("aria-checked"))
        .and_then(tristate)
        .or_else(|| attr(local_name!("aria-pressed")).and_then(tristate))
        .or_else(|| el.checkbox_input_checked().map(Toggled::from));
    if let Some(toggled) = toggled {
        builder.set_toggled(toggled);
    }

    match attr(local_name!("aria-expanded")) {
        Some("true") => builder.set_expanded(true),
        Some("false") => builder.set_expanded(false),
        _ => {}
    }

    if el.has_attr(local_name!("disabled")) || attr(local_name!("aria-disabled")) == Some("true") {
        builder.set_disabled();
    }

    let number = |name| attr(name).and_then(|v| v.parse::<f64>().ok());
    let is_range = el.name.local == local_name!("input")
        && el.attr(local_name!("type")) == Some("range");
    let now = number(local_name!("aria-valuenow"))
        .or_else(|| is_range.then(|| number(local_name!("value"))).flatten());
    if let Some(now) = now {
        builder.set_numeric_value(now);
    }
    let min = number(local_name!("aria-valuemin"))
        .or_else(|| is_range.then(|| number(local_name!("min")).or(Some(0.0))).flatten());
    if let Some(min) = min {
        builder.set_min_numeric_value(min);
    }
    let max = number(local_name!("aria-valuemax"))
        .or_else(|| is_range.then(|| number(local_name!("max")).or(Some(100.0))).flatten());
    if let Some(max) = max {
        builder.set_max_numeric_value(max);
    }

    // Text value: `aria-valuetext`, else a text field's current text. A
    // password field never exposes its text.
    if let Some(text) = attr(local_name!("aria-valuetext")) {
        builder.set_value(text);
    } else if let Some(input) = el.text_input_data()
        && el.attr(local_name!("type")) != Some("password")
    {
        builder.set_value(input.editor.raw_text());
    }
}

fn role_from_name(name: &str) -> Option<Role> {
    match name {
        "alert" => Some(Role::Alert),
        "alertdialog" => Some(Role::AlertDialog),
        "button" => Some(Role::Button),
        "checkbox" => Some(Role::CheckBox),
        "dialog" => Some(Role::Dialog),
        "gridcell" => Some(Role::GridCell),
        "link" => Some(Role::Link),
        "log" => Some(Role::Log),
        "marquee" => Some(Role::Marquee),
        "menuitem" => Some(Role::MenuItem),
        "menuitemcheckbox" => Some(Role::MenuItemCheckBox),
        "menuitemradio" => Some(Role::MenuItemRadio),
        "option" => Some(Role::ListBoxOption),
        "progressbar" => Some(Role::ProgressIndicator),
        "radio" => Some(Role::RadioButton),
        "scrollbar" => Some(Role::ScrollBar),
        "slider" => Some(Role::Slider),
        "spinbutton" => Some(Role::SpinButton),
        "status" => Some(Role::Status),
        "tab" => Some(Role::Tab),
        "tabpanel" => Some(Role::TabPanel),
        "textbox" => Some(Role::TextInput),
        "timer" => Some(Role::Timer),
        "tooltip" => Some(Role::Tooltip),
        "treeitem" => Some(Role::TreeItem),
        "combobox" => Some(Role::ComboBox),
        "grid" => Some(Role::Grid),
        "listbox" => Some(Role::ListBox),
        "menu" => Some(Role::Menu),
        "menubar" => Some(Role::MenuBar),
        "radiogroup" => Some(Role::RadioGroup),
        "tablist" => Some(Role::TabList),
        "tree" => Some(Role::Tree),
        "treegrid" => Some(Role::TreeGrid),
        "article" => Some(Role::Article),
        "columnheader" => Some(Role::ColumnHeader),
        "definition" => Some(Role::Definition),
        "document" => Some(Role::Document),
        "group" => Some(Role::Group),
        "heading" => Some(Role::Heading),
        "img" => Some(Role::Image),
        "list" => Some(Role::List),
        "listitem" => Some(Role::ListItem),
        "math" => Some(Role::Math),
        "note" => Some(Role::Note),
        "region" => Some(Role::Region),
        "row" => Some(Role::Row),
        "rowgroup" => Some(Role::RowGroup),
        "rowheader" => Some(Role::RowHeader),
        "toolbar" => Some(Role::Toolbar),
        "application" => Some(Role::Application),
        "banner" => Some(Role::Banner),
        "complementary" => Some(Role::Complementary),
        "contentinfo" => Some(Role::ContentInfo),
        "form" => Some(Role::Form),
        "main" => Some(Role::Main),
        "navigation" => Some(Role::Navigation),
        "search" => Some(Role::Search),
        _ => None,
    }
}

fn role_from_element_data(element_data: &ElementData) -> Option<Role> {
    // <https://www.w3.org/TR/html-aam-1.0/>
    match &*element_data.name.local {
        // Document structure
        "article" => Some(Role::Article),
        "aside" => Some(Role::Complementary),
        "footer" => Some(Role::Footer),
        "header" => Some(Role::Header),
        "main" => Some(Role::Main),
        "nav" => Some(Role::Navigation),
        "search" => Some(Role::Search),
        "section" => Some(Role::Section),
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => Some(Role::Heading),
        "p" => Some(Role::Paragraph),
        "blockquote" => Some(Role::Blockquote),
        "figure" => Some(Role::Figure),
        "figcaption" | "caption" => Some(Role::Caption),
        "hr" => Some(Role::Splitter),

        // Grouping
        "ul" | "ol" | "menu" => Some(Role::List),
        "li" => Some(Role::ListItem),
        "dl" => Some(Role::DescriptionList),
        "dt" => Some(Role::Term),
        "dd" => Some(Role::Definition),
        "dialog" => Some(Role::Dialog),
        "fieldset" => Some(Role::Group),
        "form" => Some(Role::Form),
        "div" => Some(Role::GenericContainer),

        // Tables
        "table" => Some(Role::Table),
        "thead" | "tbody" | "tfoot" => Some(Role::RowGroup),
        "tr" => Some(Role::Row),
        "td" => Some(Role::Cell),
        "th" => match element_data.attr(local_name!("scope")) {
            Some("row") | Some("rowgroup") => Some(Role::RowHeader),
            _ => Some(Role::ColumnHeader),
        },

        // Interactive
        // An <a> is only a link when it has an href.
        "a" => match element_data.attr(local_name!("href")) {
            Some(_) => Some(Role::Link),
            None => Some(Role::GenericContainer),
        },
        "button" => Some(Role::Button),
        "label" => Some(Role::Label),
        "legend" => Some(Role::Label),
        "select" => match element_data.attr(local_name!("multiple")) {
            Some(_) => Some(Role::ListBox),
            None => Some(Role::ComboBox),
        },
        "option" => Some(Role::ListBoxOption),
        "textarea" => Some(Role::MultilineTextInput),
        "progress" => Some(Role::ProgressIndicator),
        "meter" => Some(Role::Meter),
        "output" => Some(Role::Status),
        "summary" => Some(Role::DisclosureTriangle),

        // Inline semantics
        "code" => Some(Role::Code),
        "em" => Some(Role::Emphasis),
        "strong" => Some(Role::Strong),
        "mark" => Some(Role::Mark),
        "time" => Some(Role::Time),
        "img" => Some(Role::Image),
        "iframe" => Some(Role::Iframe),

        "input" => {
            let ty = element_data.attr(local_name!("type")).unwrap_or("text");
            match ty {
                "button" | "submit" | "reset" => Some(Role::Button),
                "checkbox" => Some(Role::CheckBox),
                "color" => Some(Role::ColorWell),
                "date" => Some(Role::DateInput),
                "datetime-local" => Some(Role::DateTimeInput),
                "email" => Some(Role::EmailInput),
                "number" => Some(Role::NumberInput),
                "password" => Some(Role::PasswordInput),
                "radio" => Some(Role::RadioButton),
                "range" => Some(Role::Slider),
                "search" => Some(Role::SearchInput),
                "tel" => Some(Role::PhoneNumberInput),
                "time" => Some(Role::TimeInput),
                _ => Some(Role::TextInput),
            }
        }
        _ => None,
    }
}
