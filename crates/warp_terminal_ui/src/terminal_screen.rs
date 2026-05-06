use std::mem;

use pathfinder_color::ColorU;
use unicode_width::UnicodeWidthChar;
use vte::{Params, Parser, Perform};
use warp_terminal::model::escape_sequences::ModeProvider;
use warp_terminal::model::{KeyboardModes, KeyboardModesApplyBehavior, TermMode};

pub struct TerminalScreen {
    parser: Parser,
    primary: ScreenBuffer,
    alternate: ScreenBuffer,
    active_buffer: ActiveBuffer,
    style: CellStyle,
    term_mode: TermMode,
    keyboard_modes: KeyboardModes,
    title: Option<String>,
    pending_writes: Vec<Vec<u8>>,
}

impl Clone for TerminalScreen {
    fn clone(&self) -> Self {
        Self {
            parser: Parser::new(),
            primary: self.primary.clone(),
            alternate: self.alternate.clone(),
            active_buffer: self.active_buffer,
            style: self.style,
            term_mode: self.term_mode,
            keyboard_modes: self.keyboard_modes,
            title: self.title.clone(),
            pending_writes: Vec::new(),
        }
    }
}

impl std::fmt::Debug for TerminalScreen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalScreen")
            .field("cols", &self.active().cols)
            .field("rows", &self.active().rows)
            .field("active_buffer", &self.active_buffer)
            .field("title", &self.title)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActiveBuffer {
    Primary,
    Alternate,
}

#[derive(Debug, Clone)]
struct ScreenBuffer {
    cols: usize,
    rows: usize,
    cells: Vec<Vec<Cell>>,
    cursor: Cursor,
    saved_cursor: Cursor,
    scroll_top: usize,
    scroll_bottom: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Cursor {
    row: usize,
    col: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cell {
    ch: char,
    style: CellStyle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CellStyle {
    fg: TerminalColor,
    bg: TerminalColor,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    inverse: bool,
    hidden: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalColor {
    DefaultForeground,
    DefaultBackground,
    Palette(u8),
    Rgb(u8, u8, u8),
}

#[derive(Debug, Clone)]
pub struct RenderedRow {
    pub cells: Vec<RenderedCell>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderedCell {
    pub ch: char,
    pub fg: ColorU,
    pub bg: ColorU,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
}

impl TerminalScreen {
    pub fn new(cols: u16, rows: u16) -> Self {
        let cols = cols.max(1) as usize;
        let rows = rows.max(1) as usize;

        Self {
            parser: Parser::new(),
            primary: ScreenBuffer::new(cols, rows),
            alternate: ScreenBuffer::new(cols, rows),
            active_buffer: ActiveBuffer::Primary,
            style: CellStyle::default(),
            term_mode: TermMode::default(),
            keyboard_modes: KeyboardModes::default(),
            title: None,
            pending_writes: Vec::new(),
        }
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        let cols = cols.max(1) as usize;
        let rows = rows.max(1) as usize;
        self.primary.resize(cols, rows);
        self.alternate.resize(cols, rows);
    }

    pub fn process(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        let mut parser = mem::replace(&mut self.parser, Parser::new());
        for byte in bytes {
            parser.advance(self, *byte);
        }
        self.parser = parser;
        mem::take(&mut self.pending_writes)
    }

    pub fn rendered_rows(&self) -> Vec<RenderedRow> {
        self.active().rendered_rows(self.term_mode)
    }

    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    pub fn input_mode(&self) -> TerminalInputMode {
        TerminalInputMode {
            term_mode: self.term_mode,
        }
    }

    fn active(&self) -> &ScreenBuffer {
        match self.active_buffer {
            ActiveBuffer::Primary => &self.primary,
            ActiveBuffer::Alternate => &self.alternate,
        }
    }

    fn active_mut(&mut self) -> &mut ScreenBuffer {
        match self.active_buffer {
            ActiveBuffer::Primary => &mut self.primary,
            ActiveBuffer::Alternate => &mut self.alternate,
        }
    }

    fn blank_cell(&self) -> Cell {
        Cell::new(' ', self.style)
    }

    fn put_char(&mut self, ch: char) {
        let width = UnicodeWidthChar::width(ch).unwrap_or(1);
        if width == 0 {
            return;
        }

        if self.active().cursor.col >= self.active().cols {
            self.wrap_or_clamp_cursor();
        }

        if width == 2 && self.active().cursor.col + 1 >= self.active().cols {
            self.carriage_return();
            self.linefeed();
        }

        let display_char = if self.style.hidden { ' ' } else { ch };
        let style = self.style;
        let buffer = self.active_mut();
        let row = buffer.cursor.row.min(buffer.rows - 1);
        let col = buffer.cursor.col.min(buffer.cols - 1);
        buffer.cells[row][col] = Cell::new(display_char, style);

        if width == 2 && col + 1 < buffer.cols {
            buffer.cells[row][col + 1] = Cell::new(' ', style);
        }

        buffer.cursor.col += width;
        if buffer.cursor.col >= buffer.cols {
            self.wrap_or_clamp_cursor();
        }
    }

    fn wrap_or_clamp_cursor(&mut self) {
        if self.term_mode.contains(TermMode::LINE_WRAP) {
            self.carriage_return();
            self.linefeed();
        } else {
            let cols = self.active().cols;
            self.active_mut().cursor.col = cols.saturating_sub(1);
        }
    }

    fn carriage_return(&mut self) {
        self.active_mut().cursor.col = 0;
    }

    fn linefeed(&mut self) {
        let blank = self.blank_cell();
        let carriage_return = self.term_mode.contains(TermMode::LINE_FEED_NEW_LINE);
        let buffer = self.active_mut();
        if buffer.cursor.row + 1 >= buffer.scroll_bottom {
            buffer.scroll_up(1, blank);
        } else {
            buffer.cursor.row = (buffer.cursor.row + 1).min(buffer.rows - 1);
        }

        if carriage_return {
            buffer.cursor.col = 0;
        }
    }

    fn reverse_index(&mut self) {
        let blank = self.blank_cell();
        let buffer = self.active_mut();
        if buffer.cursor.row <= buffer.scroll_top {
            buffer.scroll_down(1, blank);
        } else {
            buffer.cursor.row = buffer.cursor.row.saturating_sub(1);
        }
    }

    fn move_up(&mut self, count: usize) {
        let buffer = self.active_mut();
        buffer.cursor.row = buffer.cursor.row.saturating_sub(count);
    }

    fn move_down(&mut self, count: usize) {
        let buffer = self.active_mut();
        buffer.cursor.row = (buffer.cursor.row + count).min(buffer.rows - 1);
    }

    fn move_forward(&mut self, count: usize) {
        let buffer = self.active_mut();
        buffer.cursor.col = (buffer.cursor.col + count).min(buffer.cols - 1);
    }

    fn move_backward(&mut self, count: usize) {
        let buffer = self.active_mut();
        buffer.cursor.col = buffer.cursor.col.saturating_sub(count);
    }

    fn goto(&mut self, row: usize, col: usize) {
        let use_origin = self.term_mode.contains(TermMode::ORIGIN);
        let buffer = self.active_mut();
        let origin = if use_origin { buffer.scroll_top } else { 0 };
        buffer.cursor.row = (origin + row).min(buffer.rows - 1);
        buffer.cursor.col = col.min(buffer.cols - 1);
    }

    fn goto_line(&mut self, row: usize) {
        let col = self.active().cursor.col;
        self.goto(row, col);
    }

    fn goto_col(&mut self, col: usize) {
        let buffer = self.active_mut();
        buffer.cursor.col = col.min(buffer.cols - 1);
    }

    fn clear_screen(&mut self, mode: u16) {
        let blank = self.blank_cell();
        let buffer = self.active_mut();
        match mode {
            0 => {
                for row in buffer.cursor.row..buffer.rows {
                    let start = if row == buffer.cursor.row {
                        buffer.cursor.col
                    } else {
                        0
                    };
                    for col in start..buffer.cols {
                        buffer.cells[row][col] = blank;
                    }
                }
            }
            1 => {
                for row in 0..=buffer.cursor.row {
                    let end = if row == buffer.cursor.row {
                        buffer.cursor.col
                    } else {
                        buffer.cols - 1
                    };
                    for col in 0..=end {
                        buffer.cells[row][col] = blank;
                    }
                }
            }
            2 | 3 => buffer.clear(blank),
            _ => {}
        }
    }

    fn clear_line(&mut self, mode: u16) {
        let blank = self.blank_cell();
        let buffer = self.active_mut();
        let row = buffer.cursor.row.min(buffer.rows - 1);
        match mode {
            0 => {
                for col in buffer.cursor.col..buffer.cols {
                    buffer.cells[row][col] = blank;
                }
            }
            1 => {
                for col in 0..=buffer.cursor.col.min(buffer.cols - 1) {
                    buffer.cells[row][col] = blank;
                }
            }
            2 => {
                for col in 0..buffer.cols {
                    buffer.cells[row][col] = blank;
                }
            }
            _ => {}
        }
    }

    fn insert_blank(&mut self, count: usize) {
        let blank = self.blank_cell();
        let buffer = self.active_mut();
        let row = buffer.cursor.row.min(buffer.rows - 1);
        let col = buffer.cursor.col.min(buffer.cols - 1);
        let count = count.min(buffer.cols - col);
        for target_col in (col + count..buffer.cols).rev() {
            buffer.cells[row][target_col] = buffer.cells[row][target_col - count];
        }
        for target_col in col..col + count {
            buffer.cells[row][target_col] = blank;
        }
    }

    fn delete_chars(&mut self, count: usize) {
        let blank = self.blank_cell();
        let buffer = self.active_mut();
        let row = buffer.cursor.row.min(buffer.rows - 1);
        let col = buffer.cursor.col.min(buffer.cols - 1);
        let count = count.min(buffer.cols - col);
        for target_col in col..buffer.cols - count {
            buffer.cells[row][target_col] = buffer.cells[row][target_col + count];
        }
        for target_col in buffer.cols - count..buffer.cols {
            buffer.cells[row][target_col] = blank;
        }
    }

    fn erase_chars(&mut self, count: usize) {
        let blank = self.blank_cell();
        let buffer = self.active_mut();
        let row = buffer.cursor.row.min(buffer.rows - 1);
        let start = buffer.cursor.col.min(buffer.cols - 1);
        let end = (start + count).min(buffer.cols);
        for col in start..end {
            buffer.cells[row][col] = blank;
        }
    }

    fn insert_blank_lines(&mut self, count: usize) {
        let blank = self.blank_cell();
        let buffer = self.active_mut();
        buffer.insert_lines(count, blank);
    }

    fn delete_lines(&mut self, count: usize) {
        let blank = self.blank_cell();
        let buffer = self.active_mut();
        buffer.delete_lines(count, blank);
    }

    fn set_scrolling_region(&mut self, top: usize, bottom: Option<usize>) {
        let buffer = self.active_mut();
        let top = top.saturating_sub(1).min(buffer.rows - 1);
        let bottom = bottom.unwrap_or(buffer.rows).clamp(top + 1, buffer.rows);
        buffer.scroll_top = top;
        buffer.scroll_bottom = bottom;
        buffer.cursor = Cursor::default();
    }

    fn save_cursor(&mut self) {
        let buffer = self.active_mut();
        buffer.saved_cursor = buffer.cursor;
    }

    fn restore_cursor(&mut self) {
        let buffer = self.active_mut();
        buffer.cursor = buffer.saved_cursor;
        buffer.clamp_cursor();
    }

    fn reset(&mut self) {
        self.style = CellStyle::default();
        self.term_mode = TermMode::default();
        self.keyboard_modes = KeyboardModes::default();
        self.active_buffer = ActiveBuffer::Primary;
        let blank = self.blank_cell();
        self.primary.clear(blank);
        self.alternate.clear(blank);
    }

    fn set_mode(&mut self, private: bool, mode: u16) {
        match (private, mode) {
            (true, 1) => self.term_mode.insert(TermMode::APP_CURSOR),
            (true, 7) => self.term_mode.insert(TermMode::LINE_WRAP),
            (true, 25) => self.term_mode.insert(TermMode::SHOW_CURSOR),
            (true, 1000) => self.term_mode.insert(TermMode::MOUSE_REPORT_CLICK),
            (true, 1002) => self.term_mode.insert(TermMode::MOUSE_DRAG),
            (true, 1003) => self.term_mode.insert(TermMode::MOUSE_MOTION),
            (true, 1004) => self.term_mode.insert(TermMode::FOCUS_IN_OUT),
            (true, 1006) => self.term_mode.insert(TermMode::SGR_MOUSE),
            (true, 1007) => self.term_mode.insert(TermMode::ALTERNATE_SCROLL),
            (true, 47 | 1049) => {
                self.save_cursor();
                self.active_buffer = ActiveBuffer::Alternate;
                let blank = self.blank_cell();
                self.alternate.clear(blank);
            }
            (true, 2004) => self.term_mode.insert(TermMode::BRACKETED_PASTE),
            (false, 4) => self.term_mode.insert(TermMode::INSERT),
            (false, 20) => self.term_mode.insert(TermMode::LINE_FEED_NEW_LINE),
            _ => {}
        }
    }

    fn unset_mode(&mut self, private: bool, mode: u16) {
        match (private, mode) {
            (true, 1) => self.term_mode.remove(TermMode::APP_CURSOR),
            (true, 7) => self.term_mode.remove(TermMode::LINE_WRAP),
            (true, 25) => self.term_mode.remove(TermMode::SHOW_CURSOR),
            (true, 1000) => self.term_mode.remove(TermMode::MOUSE_REPORT_CLICK),
            (true, 1002) => self.term_mode.remove(TermMode::MOUSE_DRAG),
            (true, 1003) => self.term_mode.remove(TermMode::MOUSE_MOTION),
            (true, 1004) => self.term_mode.remove(TermMode::FOCUS_IN_OUT),
            (true, 1006) => self.term_mode.remove(TermMode::SGR_MOUSE),
            (true, 1007) => self.term_mode.remove(TermMode::ALTERNATE_SCROLL),
            (true, 47 | 1049) => {
                self.active_buffer = ActiveBuffer::Primary;
                self.restore_cursor();
            }
            (true, 2004) => self.term_mode.remove(TermMode::BRACKETED_PASTE),
            (false, 4) => self.term_mode.remove(TermMode::INSERT),
            (false, 20) => self.term_mode.remove(TermMode::LINE_FEED_NEW_LINE),
            _ => {}
        }
    }

    fn set_keyboard_modes(&mut self, modes: KeyboardModes, apply: KeyboardModesApplyBehavior) {
        match apply {
            KeyboardModesApplyBehavior::Replace => self.keyboard_modes = modes,
            KeyboardModesApplyBehavior::Union => self.keyboard_modes.insert(modes),
            KeyboardModesApplyBehavior::Difference => self.keyboard_modes.remove(modes),
        }
        self.sync_keyboard_term_modes();
    }

    fn sync_keyboard_term_modes(&mut self) {
        self.term_mode.remove(TermMode::KEYBOARD_PROTOCOL);
        self.term_mode.insert(TermMode::from(self.keyboard_modes));
    }

    fn apply_sgr(&mut self, params: Vec<Vec<u16>>) {
        if params.is_empty() {
            self.style = CellStyle::default();
            return;
        }

        let mut iter = params
            .iter()
            .map(|param| param.first().copied().unwrap_or(0));
        while let Some(param) = iter.next() {
            match param {
                0 => self.style = CellStyle::default(),
                1 => self.style.bold = true,
                2 => self.style.dim = true,
                3 => self.style.italic = true,
                4 => self.style.underline = true,
                7 => self.style.inverse = true,
                8 => self.style.hidden = true,
                22 => {
                    self.style.bold = false;
                    self.style.dim = false;
                }
                23 => self.style.italic = false,
                24 => self.style.underline = false,
                27 => self.style.inverse = false,
                28 => self.style.hidden = false,
                30..=37 => self.style.fg = TerminalColor::Palette((param - 30) as u8),
                39 => self.style.fg = TerminalColor::DefaultForeground,
                40..=47 => self.style.bg = TerminalColor::Palette((param - 40) as u8),
                49 => self.style.bg = TerminalColor::DefaultBackground,
                90..=97 => self.style.fg = TerminalColor::Palette((param - 90 + 8) as u8),
                100..=107 => self.style.bg = TerminalColor::Palette((param - 100 + 8) as u8),
                38 | 48 => {
                    let is_fg = param == 38;
                    match iter.next() {
                        Some(5) => {
                            if let Some(index) = iter.next() {
                                let color = TerminalColor::Palette(index.min(255) as u8);
                                if is_fg {
                                    self.style.fg = color;
                                } else {
                                    self.style.bg = color;
                                }
                            }
                        }
                        Some(2) => {
                            let r = iter.next().unwrap_or(0).min(255) as u8;
                            let g = iter.next().unwrap_or(0).min(255) as u8;
                            let b = iter.next().unwrap_or(0).min(255) as u8;
                            let color = TerminalColor::Rgb(r, g, b);
                            if is_fg {
                                self.style.fg = color;
                            } else {
                                self.style.bg = color;
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }

    fn reply(&mut self, bytes: impl Into<Vec<u8>>) {
        self.pending_writes.push(bytes.into());
    }
}

impl ModeProvider for TerminalScreen {
    fn is_term_mode_set(&self, mode: TermMode) -> bool {
        self.term_mode.contains(mode)
    }
}

#[derive(Clone, Copy)]
pub struct TerminalInputMode {
    term_mode: TermMode,
}

impl ModeProvider for TerminalInputMode {
    fn is_term_mode_set(&self, mode: TermMode) -> bool {
        self.term_mode.contains(mode)
    }
}

impl Perform for TerminalScreen {
    fn print(&mut self, c: char) {
        self.put_char(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\t' => {
                let next_tab = ((self.active().cursor.col / 8) + 1) * 8;
                self.goto_col(next_tab.min(self.active().cols - 1));
            }
            0x08 => self.move_backward(1),
            b'\r' => self.carriage_return(),
            b'\n' | 0x0b | 0x0c => self.linefeed(),
            _ => {}
        }
    }

    fn hook(&mut self, _: &Params, _: &[u8], _: bool, _: char) {}

    fn put(&mut self, _: u8) {}

    fn unhook(&mut self) {}

    fn osc_dispatch(&mut self, params: &[&[u8]], _: bool) {
        let Some(kind) = params
            .first()
            .and_then(|param| std::str::from_utf8(param).ok())
        else {
            return;
        };

        if matches!(kind, "0" | "1" | "2") {
            let title = params
                .iter()
                .skip(1)
                .map(|param| String::from_utf8_lossy(param))
                .collect::<Vec<_>>()
                .join(";");
            self.title = Some(title);
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        if ignore {
            return;
        }

        let params = params
            .iter()
            .map(|param| param.to_vec())
            .collect::<Vec<_>>();
        let private = intermediates.first() == Some(&b'?');
        let first = |default: u16| param_or(&params, 0, default);
        let second = |default: u16| param_or(&params, 1, default);

        match (action, intermediates) {
            ('@', []) => self.insert_blank(first(1) as usize),
            ('A', []) => self.move_up(first(1) as usize),
            ('B', []) | ('e', []) => self.move_down(first(1) as usize),
            ('C', []) | ('a', []) => self.move_forward(first(1) as usize),
            ('D', []) => self.move_backward(first(1) as usize),
            ('E', []) => {
                self.move_down(first(1) as usize);
                self.carriage_return();
            }
            ('F', []) => {
                self.move_up(first(1) as usize);
                self.carriage_return();
            }
            ('G', []) | ('`', []) => self.goto_col(first(1).saturating_sub(1) as usize),
            ('H', []) | ('f', []) => self.goto(
                first(1).saturating_sub(1) as usize,
                second(1).saturating_sub(1) as usize,
            ),
            ('J', []) => self.clear_screen(first(0)),
            ('K', []) => self.clear_line(first(0)),
            ('L', []) => self.insert_blank_lines(first(1) as usize),
            ('M', []) => self.delete_lines(first(1) as usize),
            ('P', []) => self.delete_chars(first(1) as usize),
            ('S', []) => {
                let blank = self.blank_cell();
                self.active_mut().scroll_up(first(1) as usize, blank);
            }
            ('T', []) => {
                let blank = self.blank_cell();
                self.active_mut().scroll_down(first(1) as usize, blank);
            }
            ('X', []) => self.erase_chars(first(1) as usize),
            ('c', _) => {
                if first(0) == 0 {
                    self.reply(b"\x1b[?1;2c".to_vec());
                }
            }
            ('d', []) => self.goto_line(first(1).saturating_sub(1) as usize),
            ('h', _) => {
                for param in mode_params(&params) {
                    self.set_mode(private, param);
                }
            }
            ('l', _) => {
                for param in mode_params(&params) {
                    self.unset_mode(private, param);
                }
            }
            ('m', []) => self.apply_sgr(params),
            ('n', []) => match first(0) {
                5 => self.reply(b"\x1b[0n".to_vec()),
                6 => {
                    let cursor = self.active().cursor;
                    self.reply(format!("\x1b[{};{}R", cursor.row + 1, cursor.col + 1));
                }
                _ => {}
            },
            ('r', []) => self.set_scrolling_region(
                first(1) as usize,
                params
                    .get(1)
                    .and_then(|param| param.first().copied())
                    .filter(|param| *param != 0)
                    .map(usize::from),
            ),
            ('s', []) => self.save_cursor(),
            ('t', []) => match first(1) {
                14 => {
                    let height = self.active().rows * 17;
                    let width = self.active().cols * 8;
                    self.reply(format!("\x1b[4;{height};{width}t"));
                }
                18 => {
                    let rows = self.active().rows;
                    let cols = self.active().cols;
                    self.reply(format!("\x1b[8;{rows};{cols}t"));
                }
                _ => {}
            },
            ('u', []) => self.restore_cursor(),
            ('u', [b'=']) => {
                let modes = KeyboardModes::from_bits_truncate(first(0) as u32);
                if let Some(apply) = KeyboardModesApplyBehavior::from_kitty_apply_mode(second(1)) {
                    self.set_keyboard_modes(modes, apply);
                }
            }
            ('u', [b'>']) => {
                let modes = KeyboardModes::from_bits_truncate(first(0) as u32);
                self.set_keyboard_modes(modes, KeyboardModesApplyBehavior::Union);
            }
            ('u', [b'<']) => {
                self.set_keyboard_modes(
                    KeyboardModes::default(),
                    KeyboardModesApplyBehavior::Replace,
                );
            }
            ('u', [b'?']) => {
                self.reply(format!("\x1b[?{}u", self.keyboard_modes.bits()));
            }
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _: bool, byte: u8) {
        match (byte, intermediates) {
            (b'D', []) => self.linefeed(),
            (b'E', []) => {
                self.linefeed();
                self.carriage_return();
            }
            (b'M', []) => self.reverse_index(),
            (b'7', []) => self.save_cursor(),
            (b'8', []) => self.restore_cursor(),
            (b'=', []) => self.term_mode.insert(TermMode::APP_KEYPAD),
            (b'>', []) => self.term_mode.remove(TermMode::APP_KEYPAD),
            (b'c', []) => self.reset(),
            _ => {}
        }
    }
}

impl ScreenBuffer {
    fn new(cols: usize, rows: usize) -> Self {
        let cells = vec![vec![Cell::default(); cols]; rows];
        Self {
            cols,
            rows,
            cells,
            cursor: Cursor::default(),
            saved_cursor: Cursor::default(),
            scroll_top: 0,
            scroll_bottom: rows,
        }
    }

    fn resize(&mut self, cols: usize, rows: usize) {
        self.cols = cols;
        self.rows = rows;
        self.cells.resize_with(rows, || vec![Cell::default(); cols]);
        for row in &mut self.cells {
            row.resize(cols, Cell::default());
        }
        self.scroll_top = 0;
        self.scroll_bottom = rows;
        self.clamp_cursor();
    }

    fn clear(&mut self, blank: Cell) {
        for row in &mut self.cells {
            row.fill(blank);
        }
        self.cursor = Cursor::default();
        self.scroll_top = 0;
        self.scroll_bottom = self.rows;
    }

    fn scroll_up(&mut self, count: usize, blank: Cell) {
        let count = count.min(self.scroll_bottom.saturating_sub(self.scroll_top));
        for _ in 0..count {
            self.cells.remove(self.scroll_top);
            self.cells
                .insert(self.scroll_bottom - 1, vec![blank; self.cols]);
        }
    }

    fn scroll_down(&mut self, count: usize, blank: Cell) {
        let count = count.min(self.scroll_bottom.saturating_sub(self.scroll_top));
        for _ in 0..count {
            self.cells.remove(self.scroll_bottom - 1);
            self.cells.insert(self.scroll_top, vec![blank; self.cols]);
        }
    }

    fn insert_lines(&mut self, count: usize, blank: Cell) {
        if self.cursor.row < self.scroll_top || self.cursor.row >= self.scroll_bottom {
            return;
        }
        let count = count.min(self.scroll_bottom - self.cursor.row);
        for _ in 0..count {
            self.cells.remove(self.scroll_bottom - 1);
            self.cells.insert(self.cursor.row, vec![blank; self.cols]);
        }
    }

    fn delete_lines(&mut self, count: usize, blank: Cell) {
        if self.cursor.row < self.scroll_top || self.cursor.row >= self.scroll_bottom {
            return;
        }
        let count = count.min(self.scroll_bottom - self.cursor.row);
        for _ in 0..count {
            self.cells.remove(self.cursor.row);
            self.cells
                .insert(self.scroll_bottom - 1, vec![blank; self.cols]);
        }
    }

    fn clamp_cursor(&mut self) {
        self.cursor.row = self.cursor.row.min(self.rows - 1);
        self.cursor.col = self.cursor.col.min(self.cols - 1);
        self.saved_cursor.row = self.saved_cursor.row.min(self.rows - 1);
        self.saved_cursor.col = self.saved_cursor.col.min(self.cols - 1);
    }

    fn rendered_rows(&self, term_mode: TermMode) -> Vec<RenderedRow> {
        self.cells
            .iter()
            .enumerate()
            .map(|(row_index, row)| RenderedRow {
                cells: row
                    .iter()
                    .enumerate()
                    .map(|(col_index, cell)| {
                        let is_cursor = term_mode.contains(TermMode::SHOW_CURSOR)
                            && row_index == self.cursor.row
                            && col_index == self.cursor.col;
                        cell.render(is_cursor)
                    })
                    .collect(),
            })
            .collect()
    }
}

impl Cell {
    fn new(ch: char, style: CellStyle) -> Self {
        Self { ch, style }
    }

    fn render(&self, is_cursor: bool) -> RenderedCell {
        let mut fg = self.style.fg.to_color();
        let mut bg = self.style.bg.to_color();
        if self.style.inverse || is_cursor {
            mem::swap(&mut fg, &mut bg);
        }
        if self.style.dim {
            fg = dim_color(fg);
        }

        RenderedCell {
            ch: self.ch,
            fg,
            bg,
            bold: self.style.bold,
            italic: self.style.italic,
            underline: self.style.underline,
        }
    }
}

impl Default for Cell {
    fn default() -> Self {
        Self::new(' ', CellStyle::default())
    }
}

impl Default for CellStyle {
    fn default() -> Self {
        Self {
            fg: TerminalColor::DefaultForeground,
            bg: TerminalColor::DefaultBackground,
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            inverse: false,
            hidden: false,
        }
    }
}

impl TerminalColor {
    fn to_color(self) -> ColorU {
        match self {
            TerminalColor::DefaultForeground => ColorU::new(238, 238, 229, 255),
            TerminalColor::DefaultBackground => ColorU::new(0, 0, 0, 255),
            TerminalColor::Rgb(r, g, b) => ColorU::new(r, g, b, 255),
            TerminalColor::Palette(index) => xterm_palette(index),
        }
    }
}

fn param_or(params: &[Vec<u16>], index: usize, default: u16) -> u16 {
    params
        .get(index)
        .and_then(|param| param.first().copied())
        .filter(|param| *param != 0)
        .unwrap_or(default)
}

fn mode_params(params: &[Vec<u16>]) -> impl Iterator<Item = u16> + '_ {
    params
        .iter()
        .filter_map(|param| param.first().copied())
        .filter(|param| *param != 0)
}

fn dim_color(color: ColorU) -> ColorU {
    ColorU::new(color.r / 2, color.g / 2, color.b / 2, color.a)
}

fn xterm_palette(index: u8) -> ColorU {
    const BASIC: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (205, 49, 49),
        (13, 188, 121),
        (229, 229, 16),
        (36, 114, 200),
        (188, 63, 188),
        (17, 168, 205),
        (229, 229, 229),
        (102, 102, 102),
        (241, 76, 76),
        (35, 209, 139),
        (245, 245, 67),
        (59, 142, 234),
        (214, 112, 214),
        (41, 184, 219),
        (255, 255, 255),
    ];

    match index {
        0..=15 => {
            let (r, g, b) = BASIC[index as usize];
            ColorU::new(r, g, b, 255)
        }
        16..=231 => {
            let value = index - 16;
            let r = value / 36;
            let g = (value % 36) / 6;
            let b = value % 6;
            ColorU::new(color_cube(r), color_cube(g), color_cube(b), 255)
        }
        232..=255 => {
            let value = 8 + (index - 232) * 10;
            ColorU::new(value, value, value, 255)
        }
    }
}

fn color_cube(value: u8) -> u8 {
    if value == 0 {
        0
    } else {
        55 + value * 40
    }
}
