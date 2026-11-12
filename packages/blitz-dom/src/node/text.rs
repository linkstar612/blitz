use blitz_traits::{
    events::{BlitzImeEvent, BlitzKeyEvent},
    node_id::NodeId,
    shell::ShellProvider,
};
use keyboard_types::{Key, Modifiers};
use parley::{ContentWidths, FontContext, LayoutContext};

use crate::util::ACTION_MOD;

/// polyvox R-PWF.4: what a clipboard chord asks of a text input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClipboardChord {
    Copy,
    Cut,
    Paste,
}

/// polyvox R-PWF.4: a paste into a single-line input, as Chromium makes it:
/// each line break (CRLF, LF or CR) becomes one space.
fn single_line_paste(text: &str) -> String {
    text.replace("\r\n", "\n").replace(['\r', '\n'], " ")
}

/// polyvox R-PWF.4: Ctrl+Right on Windows, where Chromium (and every Windows
/// text box) lands on the next word's start, past the whitespace, while
/// parley's visual word move stops at the end of the word, as macOS and GTK
/// do. `Cursor::next_logical_word` is that start; Ctrl+Delete already deletes
/// up to it. Right-to-left text keeps parley's visual move. Returns whether it
/// moved.
#[cfg(target_os = "windows")]
fn word_right_windows(driver: &mut parley::PlainEditorDriver<'_, TextBrush>, extend: bool) -> bool {
    driver.refresh_layout();
    let target = {
        let Some(layout) = driver.editor.try_layout() else {
            return false;
        };
        if layout.is_rtl() {
            return false;
        }
        driver
            .editor
            .raw_selection()
            .focus()
            .next_logical_word(layout)
            .index()
    };
    if extend {
        driver.extend_selection_to_byte(target);
    } else {
        driver.move_to_byte(target);
    }
    true
}

#[cfg(not(target_os = "windows"))]
fn word_right_windows(
    _driver: &mut parley::PlainEditorDriver<'_, TextBrush>,
    _extend: bool,
) -> bool {
    false
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
/// Parley Brush type for Blitz which contains the Blitz node id
pub struct TextBrush {
    /// The node id for the span
    pub id: NodeId,
}

impl TextBrush {
    pub(crate) fn from_id(id: NodeId) -> Self {
        Self { id }
    }
}

#[derive(Clone, Default)]
pub struct TextLayout {
    pub text: String,
    pub content_widths: Option<ContentWidths>,
    pub layout: parley::layout::Layout<TextBrush>,
    /// Block-axis offset (in CSS px) of the line boxes from the top of the container's content
    /// box, as applied by `align-content`.
    pub block_offset: f32,
    /// The out-of-flow positions for which some inline span with an inline box below it is a
    /// containing block (as `SPAN_*_CB` flags). The inline root claims those boxes in its place.
    /// Set by each `RunMode::PerformLayout` run of inline layout.
    pub span_cb_flags: stylo_taffy::StyleFlags,
    /// The `max_advance` the last PERFORM-LAYOUT pass broke `layout` into lines
    /// at, in device pixels.
    ///
    /// `layout` is the same object the painter draws from, and an intrinsic
    /// sizing round re-breaks it to answer "how tall at max-content?". Without
    /// this the last measure of the frame wins the paint: a box laid out 512px
    /// wide over two lines, then measured at its 885px max-content, is painted
    /// as one 885px line inside a 512px box, which overflows the row and runs
    /// under whatever control sits beside it. Recording the real width lets a
    /// measure put the lines back when it is done.
    pub laid_out_width: Option<f32>,
}

impl TextLayout {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn content_widths(&mut self) -> ContentWidths {
        *self
            .content_widths
            .get_or_insert_with(|| self.layout.calculate_content_widths())
    }
}

impl std::fmt::Debug for TextLayout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TextLayout")
    }
}

// TODO: support keypress events
pub enum GeneratedTextInputEvent {
    Input,
    Select,
    PreEditChange,
    Submit,
}

pub struct TextInputData {
    /// A parley TextEditor instance
    pub editor: Box<parley::PlainEditor<TextBrush>>,
    /// Whether the input is a singleline or multiline input
    pub is_multiline: bool,
    /// The scroll offset of the text content within the input, in CSS (unscaled) pixels.
    ///
    /// For single-line inputs this is a horizontal offset; for multi-line inputs it is a
    /// vertical offset. It is kept up to date so that the caret remains visible within the
    /// input's content box.
    pub scroll_offset: f32,
}

// FIXME: Implement Clone for PlainEditor
impl Clone for TextInputData {
    fn clone(&self) -> Self {
        TextInputData::new(self.is_multiline)
    }
}

impl TextInputData {
    pub fn new(is_multiline: bool) -> Self {
        let editor = Box::new(parley::PlainEditor::new(16.0));
        Self {
            editor,
            is_multiline,
            scroll_offset: 0.0,
        }
    }

    pub fn set_text(
        &mut self,
        font_ctx: &mut FontContext,
        layout_ctx: &mut LayoutContext<TextBrush>,
        text: &str,
    ) {
        if self.editor.text() != text {
            blitz_traits::text_demand::demand(text);
            self.editor.set_text(text);
            self.editor.driver(font_ctx, layout_ctx).refresh_layout();
        }
    }

    /// Recompute [`Self::scroll_offset`] so that the caret stays visible within the input's
    /// content box.
    ///
    /// `content_box_width` and `content_box_height` are the dimensions of the input's content
    /// box in CSS (unscaled) pixels.
    pub fn clamp_scroll_offset(&mut self, content_box_width: f32, content_box_height: f32) {
        let Some(layout) = self.editor.try_layout() else {
            return;
        };
        // Parley lays out at the editor's scale, so its geometry is in scaled (device) pixels.
        // We convert into CSS (unscaled) pixels to match `scroll_offset` and the content box.
        let scale = layout.scale();

        // The caret geometry relative to the start of the text content.
        let Some(caret) = self.editor.cursor_geometry(1.5) else {
            return;
        };

        // Caret bounds and content/viewport extents along the scrolling axis (CSS pixels).
        let (caret_start, caret_end, content, viewport) = if self.is_multiline {
            (
                caret.y0 as f32 / scale,
                caret.y1 as f32 / scale,
                layout.height() / scale,
                content_box_height,
            )
        } else {
            (
                caret.x0 as f32 / scale,
                caret.x1 as f32 / scale,
                layout.full_width() / scale,
                content_box_width,
            )
        };

        let mut offset = self.scroll_offset;

        // Scroll so that both edges of the caret are within the visible region.
        if caret_end > offset + viewport {
            offset = caret_end - viewport;
        }
        if caret_start < offset {
            offset = caret_start;
        }

        // Never scroll past the content, and never scroll into negative space. The content
        // extent includes the caret so that a caret at the very end remains fully visible
        // (its rendered width extends slightly past the text).
        let max_offset = (content.max(caret_end) - viewport).max(0.0);
        self.scroll_offset = offset.clamp(0.0, max_offset);
    }

    /// The maximum valid value of [`Self::scroll_offset`] (in CSS pixels) given the input's
    /// content box, i.e. the extent by which the text content overflows the content box along
    /// the input's scroll axis.
    ///
    /// `content_box_width` and `content_box_height` are the dimensions of the input's content
    /// box in CSS (unscaled) pixels.
    pub fn max_scroll_offset(&self, content_box_width: f32, content_box_height: f32) -> f32 {
        let Some(layout) = self.editor.try_layout() else {
            return 0.0;
        };
        let scale = layout.scale();
        let (content, viewport) = if self.is_multiline {
            (layout.height() / scale, content_box_height)
        } else {
            (layout.full_width() / scale, content_box_width)
        };
        (content - viewport).max(0.0)
    }

    /// Scroll the input's text content by `delta` CSS pixels along its scroll axis (horizontal
    /// for single-line inputs, vertical for multi-line inputs), clamping to the scrollable
    /// range.
    ///
    /// Returns the portion of `delta` that could not be consumed (because the input was already
    /// scrolled to its limit), so the caller can bubble it up to an ancestor scroller.
    pub fn scroll_by(
        &mut self,
        delta: f32,
        content_box_width: f32,
        content_box_height: f32,
    ) -> f32 {
        let max_offset = self.max_scroll_offset(content_box_width, content_box_height);
        if max_offset <= 0.0 {
            return delta;
        }

        // Match the sign convention used for block scrolling: a positive delta decreases the
        // scroll offset.
        let new_offset = (self.scroll_offset - delta).clamp(0.0, max_offset);
        let consumed = self.scroll_offset - new_offset;
        self.scroll_offset = new_offset;
        delta - consumed
    }

    pub(crate) fn apply_keypress_event(
        &mut self,
        font_ctx: &mut FontContext,
        layout_ctx: &mut LayoutContext<TextBrush>,
        shell_provider: &dyn ShellProvider,
        event: BlitzKeyEvent,
    ) -> Option<GeneratedTextInputEvent> {
        // Do nothing if it is a keyup event
        if !event.state.is_pressed() {
            return None;
        }

        let mods = event.modifiers;
        let shift = mods.contains(Modifiers::SHIFT);
        let action_mod = mods.contains(ACTION_MOD);

        let is_multiline = self.is_multiline;
        let editor = &mut self.editor;
        let mut driver = editor.driver(font_ctx, layout_ctx);
        // polyvox R-PWF.4: the clipboard chords Chromium binds on Windows, read
        // from its own answers (headless Edge 157). The letter matches whatever
        // its case, so Caps Lock leaves Ctrl+C a copy; Ctrl+Shift+V (paste and
        // match style) pastes; Ctrl+Shift+C and Ctrl+Shift+X bind nothing; and
        // Ctrl+Insert, Shift+Insert and Shift+Delete are copy, paste and cut.
        let clip = match &event.key {
            Key::Character(c) if action_mod => match c.to_lowercase().as_str() {
                "c" if !shift => Some(ClipboardChord::Copy),
                "x" if !shift => Some(ClipboardChord::Cut),
                "v" => Some(ClipboardChord::Paste),
                _ => None,
            },
            #[cfg(not(target_os = "macos"))]
            Key::Insert if action_mod && !shift => Some(ClipboardChord::Copy),
            #[cfg(not(target_os = "macos"))]
            Key::Insert if shift && !action_mod => Some(ClipboardChord::Paste),
            #[cfg(not(target_os = "macos"))]
            Key::Delete if shift && !action_mod => Some(ClipboardChord::Cut),
            _ => None,
        };
        if let Some(clip) = clip {
            match clip {
                ClipboardChord::Copy => {
                    if let Some(text) = driver.editor.selected_text() {
                        let _ = shell_provider.set_clipboard_text(text.to_owned());
                    }
                }
                ClipboardChord::Cut => {
                    if let Some(text) = driver.editor.selected_text() {
                        let _ = shell_provider.set_clipboard_text(text.to_owned());
                        driver.delete_selection()
                    }
                }
                ClipboardChord::Paste => {
                    let text = shell_provider.get_clipboard_text().unwrap_or_default();
                    let text = if is_multiline {
                        text
                    } else {
                        single_line_paste(&text)
                    };
                    blitz_traits::text_demand::demand(&text);
                    driver.insert_or_replace_selection(&text)
                }
            }

            return Some(GeneratedTextInputEvent::Input);
        }
        match event.key {
            Key::Character(c) if action_mod && matches!(c.to_lowercase().as_str(), "a") => {
                if shift {
                    driver.collapse_selection()
                } else {
                    driver.select_all()
                }
                return Some(GeneratedTextInputEvent::Select);
            }
            Key::ArrowLeft => {
                if action_mod {
                    if shift {
                        driver.select_word_left()
                    } else {
                        driver.move_word_left()
                    }
                } else if shift {
                    driver.select_left()
                } else {
                    driver.move_left()
                }
                return Some(GeneratedTextInputEvent::Select);
            }
            Key::ArrowRight => {
                if action_mod {
                    if !word_right_windows(&mut driver, shift) {
                        if shift {
                            driver.select_word_right()
                        } else {
                            driver.move_word_right()
                        }
                    }
                } else if shift {
                    driver.select_right()
                } else {
                    driver.move_right()
                }
                return Some(GeneratedTextInputEvent::Select);
            }
            Key::ArrowUp => {
                if shift {
                    driver.select_up()
                } else {
                    driver.move_up()
                }
                return Some(GeneratedTextInputEvent::Select);
            }
            Key::ArrowDown => {
                if shift {
                    driver.select_down()
                } else {
                    driver.move_down()
                }
                return Some(GeneratedTextInputEvent::Select);
            }
            Key::Home => {
                if action_mod {
                    if shift {
                        driver.select_to_text_start()
                    } else {
                        driver.move_to_text_start()
                    }
                } else if shift {
                    driver.select_to_line_start()
                } else {
                    driver.move_to_line_start()
                }
                return Some(GeneratedTextInputEvent::Select);
            }
            Key::End => {
                if action_mod {
                    if shift {
                        driver.select_to_text_end()
                    } else {
                        driver.move_to_text_end()
                    }
                } else if shift {
                    driver.select_to_line_end()
                } else {
                    driver.move_to_line_end()
                }
                return Some(GeneratedTextInputEvent::Select);
            }
            Key::Delete => {
                if action_mod {
                    driver.delete_word()
                } else {
                    driver.delete()
                }
                return Some(GeneratedTextInputEvent::Input);
            }

            // On macOS this is handled by the apple standard keybindings
            #[cfg(not(target_os = "macos"))]
            Key::Backspace => {
                if action_mod {
                    driver.backdelete_word()
                } else {
                    driver.backdelete()
                }
                return Some(GeneratedTextInputEvent::Input);
            }

            Key::Character(c) if c == "\n" => {
                if is_multiline {
                    driver.insert_or_replace_selection("\n");
                    return Some(GeneratedTextInputEvent::Input);
                } else {
                    return Some(GeneratedTextInputEvent::Submit);
                }
            }
            Key::Enter => {
                if is_multiline {
                    driver.insert_or_replace_selection("\n");
                    return Some(GeneratedTextInputEvent::Input);
                } else {
                    return Some(GeneratedTextInputEvent::Submit);
                }
            }
            Key::Character(s)
                if !mods.contains(Modifiers::CONTROL) && !mods.contains(Modifiers::SUPER) =>
            {
                blitz_traits::text_demand::demand(&s);
                driver.insert_or_replace_selection(&s);
                return Some(GeneratedTextInputEvent::Input);
            }
            // polyvox R-OMY.12: a character typed with SendInput's
            // KEYEVENTF_UNICODE arrives as VK_PACKET, which winit reports as
            // `Key::Unidentified` with the character only in `text` (its
            // WM_CHAR). Every other text field on Windows takes it, so this one
            // does too. Control characters stay out: Enter, Tab and Backspace
            // have their own arms when they come as real keys.
            Key::Unidentified
                if !mods.contains(Modifiers::CONTROL) && !mods.contains(Modifiers::SUPER) =>
            {
                if let Some(s) = event.text.as_deref().filter(|s| !s.chars().any(char::is_control)) {
                    blitz_traits::text_demand::demand(s);
                    driver.insert_or_replace_selection(s);
                    return Some(GeneratedTextInputEvent::Input);
                }
            }
            _ => {}
        };

        None
    }

    pub(crate) fn apply_apple_standard_keybinding(
        &mut self,
        font_ctx: &mut FontContext,
        layout_ctx: &mut LayoutContext<TextBrush>,
        shell_provider: &dyn ShellProvider,
        command: &str,
    ) -> Option<GeneratedTextInputEvent> {
        let editor = &mut self.editor;
        let mut driver = editor.driver(font_ctx, layout_ctx);
        let is_multiline = self.is_multiline;

        match command {
            // Inserting Content

            // Inserts a backtab character.
            "insertBacktab:" => {}
            // Inserts a container break, such as a new page break.
            "insertContainerBreak:" => {}
            // Inserts a double quotation mark without substituting a curly quotation mark.
            "insertDoubleQuoteIgnoringSubstitution:" => {
                driver.insert_or_replace_selection("\"");
                return Some(GeneratedTextInputEvent::Input);
            }
            // Inserts a line break character.
            "insertLineBreak:" => {
                driver.insert_or_replace_selection("\n");
                return Some(GeneratedTextInputEvent::Input);
            }
            // Inserts a newline character.
            "insertNewline:" => {
                if is_multiline {
                    driver.insert_or_replace_selection("\n");
                    return Some(GeneratedTextInputEvent::Input);
                } else {
                    return Some(GeneratedTextInputEvent::Submit);
                }
            }
            // Inserts a newline character without invoking the field editor’s normal handling to end editing.
            "insertNewlineIgnoringFieldEditor:" => {
                driver.insert_or_replace_selection("\n");
                return Some(GeneratedTextInputEvent::Input);
            }
            // Inserts a paragraph separator.
            "insertParagraphSeparator:" => {
                driver.insert_or_replace_selection("\n");
                return Some(GeneratedTextInputEvent::Input);
            }
            "insertSingleQuoteIgnoringSubstitution:" => {
                driver.insert_or_replace_selection("'");
                return Some(GeneratedTextInputEvent::Input);
            }
            // Inserts a tab character.
            "insertTab:" | "insertTabIgnoringFieldEditor:" => {
                // Ignore for now seeing as parley has poor support for laying out tabs
            }
            // Inserts the text you specify.
            "insertText:" => {}

            // Deleting Content

            // Deletes content moving backward from the current insertion point.
            // TODO: handle deleteBackwardByDecomposingPreviousCharacter separately
            "deleteBackward:" | "deleteBackwardByDecomposingPreviousCharacter:" => {
                driver.backdelete();
                return Some(GeneratedTextInputEvent::Input);
            }
            "deleteForward:" => {
                driver.delete();
                return Some(GeneratedTextInputEvent::Input);
            }
            // Deletes content from the insertion point to the beginning of the current line.
            "deleteToBeginningOfLine:" => {
                if driver.editor.raw_selection().is_collapsed() {
                    driver.select_to_line_start();
                }
                driver.delete_selection();
                return Some(GeneratedTextInputEvent::Input);
            }
            // Deletes content from the insertion point to the beginning of the current paragraph.
            "deleteToEndOfLine:" => {
                if driver.editor.raw_selection().is_collapsed() {
                    driver.select_to_line_end();
                }
                driver.delete_selection();
                return Some(GeneratedTextInputEvent::Input);
            }
            "deleteToBeginningOfParagraph:" => {
                if driver.editor.raw_selection().is_collapsed() {
                    driver.select_to_hard_line_start();
                }
                driver.delete_selection();
                return Some(GeneratedTextInputEvent::Input);
            }

            // Deletes content from the insertion point to the end of the current line.
            "deleteToEndOfParagraph:" => {
                if driver.editor.raw_selection().is_collapsed() {
                    driver.select_to_hard_line_end();
                }
                driver.delete_selection();
                return Some(GeneratedTextInputEvent::Input);
            }
            // Deletes content from the insertion point to the end of the current paragraph.
            "deleteWordBackward:" => {
                driver.backdelete_word();
                return Some(GeneratedTextInputEvent::Input);
            }
            // Deletes the word preceding the current insertion point.
            "deleteWordForward:" => {
                driver.delete_word();
                return Some(GeneratedTextInputEvent::Input);
            }
            // Deletes the current selection, placing it in a temporary buffer, such as the Clipboard.
            "yank:" => {
                if let Some(text) = driver.editor.selected_text() {
                    let _ = shell_provider.set_clipboard_text(text.to_owned());
                    driver.delete_selection();
                    return Some(GeneratedTextInputEvent::Input);
                }
            }

            // Moving the Insertion Pointer

            // Moves the insertion pointer backward in the current content.
            "moveBackward:" => {
                driver.move_left(); // TODO: Bidi-aware
                return Some(GeneratedTextInputEvent::Select);
            }

            // Moves the insertion pointer down in the current content.
            "moveDown:" => {
                driver.move_down();
                return Some(GeneratedTextInputEvent::Select);
            }
            // Moves the insertion pointer forward in the current content.
            "moveForward:" => {
                driver.move_right();
                return Some(GeneratedTextInputEvent::Select);
            } // TODO: Bidi-aware

            // Moves the insertion pointer left in the current content.
            "moveLeft:" => {
                driver.move_left();
                return Some(GeneratedTextInputEvent::Select);
            }
            // Moves the insertion pointer right in the current content.
            "moveRight:" => {
                driver.move_right();
                return Some(GeneratedTextInputEvent::Select);
            }
            // Moves the insertion pointer up in the current content.
            "moveUp:" => {
                driver.move_up();
                return Some(GeneratedTextInputEvent::Select);
            }

            // Modifying the Selection

            // Extends the selection to include the content before the current selection.
            "moveBackwardAndModifySelection:" => {
                driver.select_left(); // TODO: Bidi-aware
                return Some(GeneratedTextInputEvent::Select);
            }
            // Extends the selection to include the content below the current selection.
            "moveDownAndModifySelection:" => {
                driver.select_down();
                return Some(GeneratedTextInputEvent::Select);
            }
            // Extends the selection to include the content after the current selection.
            "moveForwardAndModifySelection:" => {
                driver.select_right(); // TODO: Bidi-aware
                return Some(GeneratedTextInputEvent::Select);
            }
            // Extends the selection to include the content to the left of the current selection.
            "moveLeftAndModifySelection:" => {
                driver.select_left();
                return Some(GeneratedTextInputEvent::Select);
            }
            // Extends the selection to include the content to the right of the current selection.
            "moveRightAndModifySelection:" => {
                driver.select_right();
                return Some(GeneratedTextInputEvent::Select);
            }
            // Extends the selection to include the content above the current selection.
            "moveUpAndModifySelection:" => {
                driver.select_up();
                return Some(GeneratedTextInputEvent::Select);
            }

            // Changing the Selection
            "selectAll:" => {
                driver.select_all();
                return Some(GeneratedTextInputEvent::Select);
            }
            "selectLine:" => {
                driver.move_to_line_start();
                driver.select_to_line_end();
                return Some(GeneratedTextInputEvent::Select);
            }
            "selectParagraph:" => {
                driver.move_to_hard_line_start();
                driver.select_to_hard_line_end();
                return Some(GeneratedTextInputEvent::Select);
            }
            "selectWord:" => {
                // TODO
            }

            // Moving the Selection in Documents
            "moveToBeginningOfDocument:" => {
                driver.move_to_text_start();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveToBeginningOfDocumentAndModifySelection:" => {
                driver.select_to_text_start();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveToEndOfDocument:" => {
                driver.move_to_text_end();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveToEndOfDocumentAndModifySelection:" => {
                driver.move_to_text_end();
                return Some(GeneratedTextInputEvent::Select);
            }

            // Moving the Selection in Paragraphs
            "moveParagraphBackwardAndModifySelection:" => {}
            "moveParagraphForwardAndModifySelection:" => {}
            "moveToBeginningOfParagraph:" => {
                driver.move_to_hard_line_start();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveToBeginningOfParagraphAndModifySelection:" => {
                driver.select_to_hard_line_start();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveToEndOfParagraph:" => {
                driver.move_to_hard_line_end();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveToEndOfParagraphAndModifySelection:" => {
                driver.select_to_hard_line_end();
                return Some(GeneratedTextInputEvent::Select);
            }

            // Moving the Selection in Lines of Text
            "moveToBeginningOfLine:" => {
                driver.move_to_line_start();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveToBeginningOfLineAndModifySelection:" => {
                driver.select_to_line_start();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveToEndOfLine:" => {
                driver.move_to_line_end();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveToEndOfLineAndModifySelection:" => {
                driver.select_to_line_end();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveToLeftEndOfLine:" => {
                driver.move_to_text_start();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveToLeftEndOfLineAndModifySelection:" => {
                driver.select_to_line_start();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveToRightEndOfLine:" => {
                driver.move_to_line_end();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveToRightEndOfLineAndModifySelection:" => {
                driver.select_to_line_end();
                return Some(GeneratedTextInputEvent::Select);
            }

            // Moving the Selection by Word Boundaries
            "moveWordBackward:" => {
                driver.move_word_left();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveWordBackwardAndModifySelection:" => {
                driver.select_word_left();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveWordForward:" => {
                driver.move_word_right();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveWordForwardAndModifySelection:" => {
                driver.select_word_right();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveWordLeft:" => {
                driver.move_word_left();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveWordLeftAndModifySelection:" => {
                driver.select_word_left();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveWordRight:" => {
                driver.move_word_right();
                return Some(GeneratedTextInputEvent::Select);
            }
            "moveWordRightAndModifySelection:" => {
                driver.select_word_right();
                return Some(GeneratedTextInputEvent::Select);
            }

            // Scrolling Content

            // Scrolls the content down by a page.
            "scrollPageDown:" => {}
            // Scrolls the content up by a page.
            "scrollPageUp:" => {}
            // Scrolls the content down by a line.
            "scrollLineDown:" => {}
            // Scrolls the content up by a line.
            "scrollLineUp:" => {}
            // Scrolls the content to the beginning of the document.
            "scrollToBeginningOfDocument:" => {}
            // Scrolls the content to the end of the document.
            "scrollToEndOfDocument:" => {}
            // Moves the visible content region down by a page.
            "pageDown:" => {}
            // Moves the visible content region up by a page.
            "pageUp:" => {}
            // Moves the visible content region down by a page, and extends the current selection.
            "pageDownAndModifySelection:" => {}
            // Moves the visible content region up by a page, and extends the current selection.
            "pageUpAndModifySelection:" => {}
            // Moves the visible content region so the current selection is visually centered.
            "centerSelectionInVisibleArea:" => {}

            // Transposing Elements

            // Transposes the content around the current selection.
            "transpose:" => {}
            // Transposes the words around the current selection.
            "transposeWords:" => {}

            // Indenting Content
            // Indents the content at the current selection.
            "indent:" => {}

            // Canceling Operations
            // Cancels the current operation.
            "cancelOperation:" => {}

            // Supporting QuickLook
            // Invokes QuickLook to preview the current selection.
            "quickLookPreviewItems:" => {}

            // Supporting Writing Directions
            "makeBaseWritingDirectionLeftToRight:" => {}
            "makeBaseWritingDirectionNatural:" => {}
            "makeBaseWritingDirectionRightToLeft:" => {}
            "makeTextWritingDirectionLeftToRight:" => {}
            "makeTextWritingDirectionNatural:" => {}
            "makeTextWritingDirectionRightToLeft:" => {}

            // Changing Capitalization
            "capitalizeWord:" => {}
            "changeCaseOfLetter:" => {}
            "lowercaseWord:" => {}
            "uppercaseWord:" => {}

            // Supporting Marked Selections
            "setMark:" => {}
            "selectToMark:" => {}
            "deleteToMark:" => {}
            "swapWithMark:" => {}

            // Supporting Autocomplete
            "complete:" => {}

            // Instance Methods
            "showContextMenuForSelection:" => {}

            // Unknown command
            _ => {}
        };

        None
    }

    pub(crate) fn apply_ime_event(
        &mut self,
        font_ctx: &mut FontContext,
        layout_ctx: &mut LayoutContext<TextBrush>,
        event: BlitzImeEvent,
    ) -> Option<GeneratedTextInputEvent> {
        let editor = &mut self.editor;
        let mut driver = editor.driver(font_ctx, layout_ctx);

        match event {
            BlitzImeEvent::Enabled => {
                // Do nothing
                None
            }
            BlitzImeEvent::Disabled => {
                driver.clear_compose();
                Some(GeneratedTextInputEvent::PreEditChange)
            }
            BlitzImeEvent::Commit(text) => {
                blitz_traits::text_demand::demand(&text);
                driver.insert_or_replace_selection(&text);
                Some(GeneratedTextInputEvent::Input)
            }
            BlitzImeEvent::Preedit(text, cursor) => {
                if text.is_empty() {
                    driver.clear_compose();
                } else {
                    blitz_traits::text_demand::demand(&text);
                    driver.set_compose(&text, cursor);
                }
                Some(GeneratedTextInputEvent::PreEditChange)
            }
            BlitzImeEvent::DeleteSurrounding {
                before_bytes,
                after_bytes,
            } => {
                let _ = before_bytes;
                let _ = after_bytes;
                // TODO
                None
            }
        }
    }
}

/// polyvox R-PWF.4: the clipboard chords, Windows' Ctrl+Right and a
/// single-line paste, against Chromium's own answers for an
/// `<input type=text>` holding `alpha beta-gamma delta` (headless Edge 157).
#[cfg(test)]
mod field_keys_tests {
    use std::sync::Mutex;

    use blitz_traits::events::{BlitzKeyEvent, KeyState};
    use blitz_traits::shell::{ClipboardError, ShellProvider};
    use keyboard_types::{Code, Key, Location, Modifiers};
    use parley::{FontContext, LayoutContext};

    use super::{TextBrush, TextInputData, single_line_paste};

    const V: &str = "alpha beta-gamma delta";

    #[derive(Default)]
    struct Clipboard(Mutex<Option<String>>);

    impl ShellProvider for Clipboard {
        fn get_clipboard_text(&self) -> Result<String, ClipboardError> {
            self.0.lock().unwrap().clone().ok_or(ClipboardError)
        }
        fn set_clipboard_text(&self, text: String) -> Result<(), ClipboardError> {
            *self.0.lock().unwrap() = Some(text);
            Ok(())
        }
    }

    struct Field {
        input: TextInputData,
        fonts: FontContext,
        layout: LayoutContext<TextBrush>,
        clip: Clipboard,
    }

    impl Field {
        fn new(text: &str) -> Self {
            let mut f = Self {
                input: TextInputData::new(false),
                fonts: FontContext::new(),
                layout: LayoutContext::new(),
                clip: Clipboard::default(),
            };
            f.input.set_text(&mut f.fonts, &mut f.layout, text);
            f
        }

        fn select(&mut self, anchor: usize, focus: usize) {
            let mut d = self.input.editor.driver(&mut self.fonts, &mut self.layout);
            d.move_to_byte(anchor);
            d.extend_selection_to_byte(focus);
        }

        fn key(&mut self, key: Key, modifiers: Modifiers) {
            let event = BlitzKeyEvent {
                key,
                code: Code::Unidentified,
                modifiers,
                location: Location::Standard,
                is_auto_repeating: false,
                is_composing: false,
                state: KeyState::Pressed,
                text: None,
            };
            let _ = self.input.apply_keypress_event(
                &mut self.fonts,
                &mut self.layout,
                &self.clip,
                event,
            );
        }

        fn text(&self) -> String {
            self.input.editor.raw_text().to_string()
        }

        /// (low, high) byte ends of the selection.
        fn sel(&self) -> (usize, usize) {
            let r = self.input.editor.raw_selection().text_range();
            (r.start, r.end)
        }

        fn clip(&self) -> Option<String> {
            self.clip.0.lock().unwrap().clone()
        }

        fn set_clip(&self, text: &str) {
            *self.clip.0.lock().unwrap() = Some(text.to_string());
        }
    }

    fn ch(c: &str) -> Key {
        Key::Character(c.into())
    }

    const CTRL: Modifiers = Modifiers::CONTROL;
    const SHIFT: Modifiers = Modifiers::SHIFT;

    #[test]
    fn caps_lock_keeps_the_clipboard_letters() {
        let mut f = Field::new(V);
        f.select(0, 5);
        f.key(ch("C"), CTRL);
        assert_eq!(
            f.clip().as_deref(),
            Some("alpha"),
            "Ctrl+C with Caps Lock copies"
        );
        f.select(0, 5);
        f.key(ch("X"), CTRL);
        assert_eq!(f.text(), " beta-gamma delta", "Ctrl+X with Caps Lock cuts");
        f.set_clip("PASTE");
        f.key(ch("V"), CTRL);
        assert_eq!(
            f.text(),
            "PASTE beta-gamma delta",
            "Ctrl+V with Caps Lock pastes"
        );
    }

    #[test]
    fn ctrl_shift_v_pastes_and_ctrl_shift_c_does_nothing() {
        let mut f = Field::new(V);
        f.set_clip("PASTE");
        f.select(5, 5);
        f.key(ch("V"), CTRL | SHIFT);
        assert_eq!(f.text(), "alphaPASTE beta-gamma delta");
        let mut f = Field::new(V);
        f.set_clip("SENTINEL");
        f.select(0, 5);
        f.key(ch("C"), CTRL | SHIFT);
        f.key(ch("X"), CTRL | SHIFT);
        assert_eq!(f.clip().as_deref(), Some("SENTINEL"));
        assert_eq!(f.text(), V);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn the_insert_and_delete_clipboard_keys() {
        let mut f = Field::new(V);
        f.set_clip("SENTINEL");
        f.select(0, 5);
        f.key(Key::Insert, CTRL);
        assert_eq!(f.clip().as_deref(), Some("alpha"), "Ctrl+Insert copies");
        f.set_clip("PASTE");
        f.select(5, 5);
        f.key(Key::Insert, SHIFT);
        assert_eq!(
            f.text(),
            "alphaPASTE beta-gamma delta",
            "Shift+Insert pastes"
        );

        let mut f = Field::new(V);
        f.select(0, 5);
        f.key(Key::Delete, SHIFT);
        assert_eq!(
            (f.text().as_str(), f.clip().as_deref()),
            (" beta-gamma delta", Some("alpha")),
            "Shift+Delete cuts"
        );
        let mut f = Field::new(V);
        f.set_clip("SENTINEL");
        f.select(5, 5);
        f.key(Key::Delete, SHIFT);
        assert_eq!(
            (f.text().as_str(), f.clip().as_deref()),
            (V, Some("SENTINEL")),
            "with nothing selected it does nothing"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn ctrl_right_lands_on_the_next_word_start() {
        let mut f = Field::new(V);
        f.select(0, 0);
        f.key(Key::ArrowRight, CTRL);
        assert_eq!(f.sel(), (6, 6), "past the space to beta");
        f.key(Key::ArrowRight, CTRL);
        assert_eq!(f.sel(), (10, 10), "the hyphen is a stop");
        f.key(Key::ArrowRight, CTRL);
        assert_eq!(f.sel(), (11, 11));
        f.select(5, 5);
        f.key(Key::ArrowRight, CTRL);
        assert_eq!(
            f.sel(),
            (6, 6),
            "from the end of a word, the next word's start"
        );
        f.select(0, 0);
        f.key(Key::ArrowRight, CTRL | SHIFT);
        assert_eq!(f.sel(), (0, 6), "Ctrl+Shift+Right selects to it");
        // Ctrl+Left was already Chromium's.
        f.select(22, 22);
        f.key(Key::ArrowLeft, CTRL);
        assert_eq!(f.sel(), (17, 17));
    }

    #[test]
    fn a_single_line_paste_turns_line_breaks_into_spaces() {
        assert_eq!(single_line_paste("a\nb"), "a b");
        assert_eq!(single_line_paste("a\r\nb\rc"), "a b c");
        let mut f = Field::new(V);
        f.set_clip("a\r\nb");
        f.select(5, 5);
        f.key(ch("v"), CTRL);
        assert_eq!(f.text(), "alphaa b beta-gamma delta");
        let mut area = Field::new("");
        area.input.is_multiline = true;
        area.set_clip("a\nb");
        area.key(ch("v"), CTRL);
        assert_eq!(area.text(), "a\nb", "a textarea keeps its line breaks");
    }
}
