//! Native TUI mouse drag-selection highlight and wide-character-safe buffer text extraction.
//!
//! Lets users click-and-drag directly inside the TUI (while keeping `EnableMouseCapture`
//! smooth-wheel scrolling active) to highlight any text span in warm cream/sand and
//! automatically copy it to the system clipboard on mouse release.

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier, Style};
use unicode_width::UnicodeWidthStr;

const SEL_BG: Color = Color::Rgb(228, 209, 167);
const SEL_FG: Color = Color::Rgb(28, 32, 34);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CellPos {
    pub row: u16,
    pub col: u16,
}

#[derive(Debug, Default, Clone)]
pub struct TextSelection {
    anchor: Option<CellPos>,
    head: Option<CellPos>,
    dragging: bool,
}

pub enum MouseSelectAction {
    None,
    Scroll(i32),
    Copied(String),
}

impl TextSelection {
    pub fn clear(&mut self) {
        self.anchor = None;
        self.head = None;
        self.dragging = false;
    }

    pub fn has_range(&self) -> bool {
        matches!((self.anchor, self.head), (Some(a), Some(b)) if a != b)
    }

    fn ordered_bounds(&self) -> Option<(CellPos, CellPos)> {
        let (a, b) = (self.anchor?, self.head?);
        if a == b {
            return None;
        }
        if a <= b {
            Some((a, b))
        } else {
            Some((b, a))
        }
    }

    /// Process a crossterm mouse event; returns `Copied(text)` on left-button release
    /// when a non-empty character range was dragged across `screen`.
    pub fn on_mouse(&mut self, ev: MouseEvent, screen: Option<&Buffer>) -> MouseSelectAction {
        let pos = CellPos {
            row: ev.row,
            col: ev.column,
        };
        match ev.kind {
            MouseEventKind::ScrollUp => {
                self.clear();
                MouseSelectAction::Scroll(-4)
            }
            MouseEventKind::ScrollDown => {
                self.clear();
                MouseSelectAction::Scroll(4)
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.anchor = Some(pos);
                self.head = Some(pos);
                self.dragging = true;
                MouseSelectAction::None
            }
            MouseEventKind::Drag(MouseButton::Left) if self.dragging => {
                self.head = Some(pos);
                MouseSelectAction::None
            }
            MouseEventKind::Up(MouseButton::Left) if self.dragging => {
                self.dragging = false;
                self.head = Some(pos);
                if let Some(text) = self.extract_text(screen) {
                    if !text.is_empty() {
                        return MouseSelectAction::Copied(text);
                    }
                }
                self.clear();
                MouseSelectAction::None
            }
            _ => MouseSelectAction::None,
        }
    }

    /// Paint warm cream/sand highlight across all cells covered by the current selection,
    /// snapping wide CJK glyphs cleanly so 2-column characters never tear in half.
    pub fn paint(&self, buf: &mut Buffer) {
        let Some((start, end)) = self.ordered_bounds() else {
            return;
        };
        let area = buf.area;
        if area.width == 0 || area.height == 0 {
            return;
        }
        let bottom = area.y.saturating_add(area.height);
        let right = area.x.saturating_add(area.width);
        let y_lo = start.row.max(area.y);
        let y_hi = end.row.min(bottom.saturating_sub(1));
        if y_lo > y_hi {
            return;
        }
        let sel_style = Style::default()
            .fg(SEL_FG)
            .bg(SEL_BG)
            .remove_modifier(Modifier::DIM);
        for y in y_lo..=y_hi {
            let col_lo = if y == start.row {
                start.col.max(area.x)
            } else {
                area.x
            };
            let col_hi = if y == end.row {
                end.col.min(right.saturating_sub(1))
            } else {
                right.saturating_sub(1)
            };
            if col_lo > col_hi {
                continue;
            }
            let mut x = area.x;
            while x < right {
                let sym = buf[(x, y)].symbol();
                let w = UnicodeWidthStr::width(sym).max(1) as u16;
                let cell_end = x.saturating_add(w - 1).min(right.saturating_sub(1));
                if cell_end >= col_lo && x <= col_hi {
                    for cx in x..=cell_end {
                        buf[(cx, y)].set_style(sel_style);
                    }
                }
                x = x.saturating_add(w);
            }
        }
    }

    /// Extract selected text from `buf`, skipping trailing spacer cells of 2-column CJK
    /// characters so copied text never contains phantom spaces.
    pub fn extract_text(&self, screen: Option<&Buffer>) -> Option<String> {
        let buf = screen?;
        let (start, end) = self.ordered_bounds()?;
        let area = buf.area;
        if area.width == 0 || area.height == 0 {
            return None;
        }
        let bottom = area.y.saturating_add(area.height);
        let right = area.x.saturating_add(area.width);
        let y_lo = start.row.max(area.y);
        let y_hi = end.row.min(bottom.saturating_sub(1));
        if y_lo > y_hi {
            return None;
        }
        let mut lines = Vec::with_capacity((y_hi - y_lo + 1) as usize);
        for y in y_lo..=y_hi {
            let col_lo = if y == start.row {
                start.col.max(area.x)
            } else {
                area.x
            };
            let col_hi = if y == end.row {
                end.col.min(right.saturating_sub(1))
            } else {
                right.saturating_sub(1)
            };
            if col_lo > col_hi {
                lines.push(String::new());
                continue;
            }
            let mut row_text = String::new();
            let mut x = area.x;
            while x < right {
                let sym = buf[(x, y)].symbol();
                let w = UnicodeWidthStr::width(sym).max(1) as u16;
                let cell_end = x.saturating_add(w - 1).min(right.saturating_sub(1));
                if cell_end >= col_lo && x <= col_hi {
                    row_text.push_str(if sym.is_empty() { " " } else { sym });
                }
                x = x.saturating_add(w);
            }
            lines.push(row_text.trim_end().to_owned());
        }
        Some(lines.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use ratatui::layout::Rect;

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn cjk_and_ascii_drag_selection_extracts_exact_text_and_highlights_wide_cells() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 3));
        // "集中"(0..4) + "在未提交的 TUI 代码引"(4..24) + "用未导"(24..30)
        buf.set_string(0, 0, "集中在未提交的 TUI 代码引用未导", Style::default());
        buf.set_string(0, 1, "second line here", Style::default());

        let clean = buf.clone();
        let mut sel = TextSelection::default();

        // Drag forward across "在未提交的 TUI 代码引" (col 4..=24 on row 0; "引" occupies 23..=24)
        sel.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 4, 0), Some(&clean));
        sel.on_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 23, 0), Some(&clean));
        assert!(sel.has_range());

        let mut painted = clean.clone();
        sel.paint(&mut painted);
        // Both halves of wide CJK glyph "在" (4, 5) and "引" (23, 24) are highlighted; "中" (2, 3) and "用" (25, 26) are not
        assert_eq!(painted[(3, 0)].bg, Color::Reset);
        assert_eq!(painted[(4, 0)].bg, SEL_BG);
        assert_eq!(painted[(5, 0)].bg, SEL_BG);
        assert_eq!(painted[(23, 0)].bg, SEL_BG);
        assert_eq!(painted[(24, 0)].bg, SEL_BG);
        assert_eq!(painted[(25, 0)].bg, Color::Reset);

        match sel.on_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 23, 0), Some(&clean)) {
            MouseSelectAction::Copied(text) => assert_eq!(text, "在未提交的 TUI 代码引"),
            _ => panic!("expected Copied on mouse up"),
        }

        // Single click without drag clears selection instead of copying
        sel.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 5, 0), Some(&clean));
        assert!(matches!(
            sel.on_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 5, 0), Some(&clean)),
            MouseSelectAction::None
        ));
        assert!(!sel.has_range());
    }
}
