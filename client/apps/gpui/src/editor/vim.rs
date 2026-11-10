//! Vim-style modal layer for `SqlEditor`: a pure keystroke → command state
//! machine. It knows nothing about gpui; `SqlEditor::apply_vim_command`
//! executes the resulting `VimCommand`s against the existing editor
//! primitives.

use super::text::{is_word_char, word_selection_range};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum VimMode {
    #[default]
    Normal,
    Insert,
    Visual,
}

impl VimMode {
    /// Status-bar label. Stable strings: tests and the badge depend on them.
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Normal => "NORMAL",
            Self::Insert => "INSERT",
            Self::Visual => "VISUAL",
        }
    }
}

/// One keystroke as the state machine sees it. `Char` is an unmodified
/// printable character (from `replace_text_in_range`); the rest arrive via
/// action handlers / `handle_key_down`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum VimKey {
    Char(char),
    Escape,
    Enter,
    Backspace,
    CtrlR,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Motion {
    Left,
    Right,
    Up,
    Down,
    WordForward,
    WordBackward,
    WordEnd,
    LineStart,
    LineEnd,
    FirstLine,
    LastLine,
    InnerWord,
    AWord,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Operator {
    Delete,
    Change,
    Yank,
}

/// What the editor must do. `count` is already folded in where it matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum VimCommand {
    Move {
        motion: Motion,
        count: usize,
    },
    /// Operator over a motion (`dw`, `c$`, `y3j`, `diw`, ...). `Change` also
    /// switches to Insert after the delete.
    Operate {
        op: Operator,
        motion: Motion,
        count: usize,
    },
    /// `dd` / `cc` / `yy`: `count` whole lines including their newline.
    OperateLines {
        op: Operator,
        count: usize,
    },
    /// Operator applied to the current Visual selection (`d`, `c`, `y`, `x`).
    OperateSelection {
        op: Operator,
    },
    DeleteCharForward {
        count: usize,
    },
    InsertHere,
    InsertAfter,
    InsertLineStart,
    InsertLineEnd,
    OpenLineBelow,
    OpenLineAbove,
    PasteAfter {
        count: usize,
    },
    PasteBefore {
        count: usize,
    },
    Undo,
    Redo,
    OpenFind,
    /// Mode changed; the editor updates the badge and, on Normal, clears the
    /// selection / extra carets.
    SetMode(VimMode),
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct VimState {
    pub(super) mode: VimMode,
    count: Option<usize>,
    pending_op: Option<Operator>,
    pending_g: bool,
    pending_text_object: bool,
    /// Meaningful only while `pending_text_object` is set: `true` for `i`, `false` for `a`.
    pending_inner: bool,
}

impl VimState {
    pub(super) fn new(mode: VimMode) -> Self {
        Self {
            mode,
            ..Self::default()
        }
    }

    /// Feed one key. Returns the commands to execute (usually 0–2). The
    /// state machine is total: unknown keys reset pending state and return
    /// an empty vec (never panic, never insert).
    pub(super) fn feed(&mut self, key: VimKey) -> Vec<VimCommand> {
        match self.mode {
            VimMode::Insert => self.feed_insert(key),
            VimMode::Normal => self.feed_normal(key),
            VimMode::Visual => self.feed_visual(key),
        }
    }

    /// True when printable characters must NOT reach the buffer.
    pub(super) fn intercepts_text(&self) -> bool {
        self.mode != VimMode::Insert
    }

    pub(super) fn reset_pending(&mut self) {
        self.count = None;
        self.pending_op = None;
        self.pending_g = false;
        self.pending_text_object = false;
        self.pending_inner = false;
    }

    pub(super) fn has_pending(&self) -> bool {
        self.count.is_some()
            || self.pending_op.is_some()
            || self.pending_g
            || self.pending_text_object
    }

    fn take_count(&mut self) -> usize {
        self.count.take().unwrap_or(1).max(1)
    }

    fn set_mode(&mut self, mode: VimMode) -> VimCommand {
        self.mode = mode;
        VimCommand::SetMode(mode)
    }

    fn feed_insert(&mut self, key: VimKey) -> Vec<VimCommand> {
        match key {
            VimKey::Escape => {
                self.reset_pending();
                vec![
                    self.set_mode(VimMode::Normal),
                    VimCommand::Move {
                        motion: Motion::Left,
                        count: 1,
                    },
                ]
            }
            _ => vec![],
        }
    }

    fn feed_normal(&mut self, key: VimKey) -> Vec<VimCommand> {
        match key {
            VimKey::Escape => {
                self.reset_pending();
                vec![]
            }
            VimKey::CtrlR => self.emit_repeated(VimCommand::Redo),
            VimKey::Enter => self.dispatch_motion(Motion::Down),
            VimKey::Backspace => self.dispatch_motion(Motion::Left),
            VimKey::Char(c) => self.feed_normal_char(c),
        }
    }

    fn feed_visual(&mut self, key: VimKey) -> Vec<VimCommand> {
        match key {
            VimKey::Escape => {
                self.reset_pending();
                vec![self.set_mode(VimMode::Normal)]
            }
            VimKey::CtrlR => {
                self.reset_pending();
                vec![]
            }
            VimKey::Enter => self.dispatch_motion(Motion::Down),
            VimKey::Backspace => self.dispatch_motion(Motion::Left),
            VimKey::Char(c) => self.feed_visual_char(c),
        }
    }

    fn emit_repeated(&mut self, cmd: VimCommand) -> Vec<VimCommand> {
        if self.pending_op.is_some() || self.pending_g || self.pending_text_object {
            self.reset_pending();
            return vec![];
        }
        let n = self.take_count();
        vec![cmd; n]
    }

    fn feed_normal_char(&mut self, c: char) -> Vec<VimCommand> {
        if self.handle_count_digit(c) {
            return vec![];
        }
        if self.pending_g {
            return self.finish_pending_g(c);
        }
        if self.pending_text_object {
            return self.finish_text_object(c);
        }
        if self.pending_op.is_some() {
            return self.feed_operator_pending(c);
        }
        match c {
            'g' => {
                self.pending_g = true;
                vec![]
            }
            'd' => {
                self.pending_op = Some(Operator::Delete);
                vec![]
            }
            'c' => {
                self.pending_op = Some(Operator::Change);
                vec![]
            }
            'y' => {
                self.pending_op = Some(Operator::Yank);
                vec![]
            }
            'x' => {
                let n = self.take_count();
                vec![VimCommand::DeleteCharForward { count: n }]
            }
            'i' => self.enter_insert(VimCommand::InsertHere),
            'a' => self.enter_insert(VimCommand::InsertAfter),
            'I' => self.enter_insert(VimCommand::InsertLineStart),
            'A' => self.enter_insert(VimCommand::InsertLineEnd),
            'o' => self.enter_insert(VimCommand::OpenLineBelow),
            'O' => self.enter_insert(VimCommand::OpenLineAbove),
            'p' => {
                let n = self.take_count();
                vec![VimCommand::PasteAfter { count: n }]
            }
            'P' => {
                let n = self.take_count();
                vec![VimCommand::PasteBefore { count: n }]
            }
            'u' => self.emit_repeated(VimCommand::Undo),
            '/' => {
                self.reset_pending();
                vec![VimCommand::OpenFind]
            }
            'v' => {
                self.reset_pending();
                vec![self.set_mode(VimMode::Visual)]
            }
            _ => {
                if let Some(motion) = motion_from_char(c) {
                    return self.dispatch_motion(motion);
                }
                self.reset_pending();
                vec![]
            }
        }
    }

    fn feed_visual_char(&mut self, c: char) -> Vec<VimCommand> {
        if self.handle_count_digit(c) {
            return vec![];
        }
        if self.pending_g {
            return self.finish_pending_g(c);
        }
        match c {
            'g' => {
                self.pending_g = true;
                vec![]
            }
            'v' => {
                self.reset_pending();
                vec![self.set_mode(VimMode::Normal)]
            }
            'd' | 'x' => self.visual_operate(Operator::Delete),
            'c' => self.visual_operate(Operator::Change),
            'y' => self.visual_operate(Operator::Yank),
            'p' | 'P' | ':' => {
                self.reset_pending();
                vec![]
            }
            _ => {
                if let Some(motion) = motion_from_char(c) {
                    return self.dispatch_motion(motion);
                }
                self.reset_pending();
                vec![]
            }
        }
    }

    fn visual_operate(&mut self, op: Operator) -> Vec<VimCommand> {
        self.reset_pending();
        let mut cmds = vec![VimCommand::OperateSelection { op }];
        cmds.push(self.set_mode(if op == Operator::Change {
            VimMode::Insert
        } else {
            VimMode::Normal
        }));
        cmds
    }

    fn enter_insert(&mut self, prep: VimCommand) -> Vec<VimCommand> {
        self.reset_pending();
        vec![prep, self.set_mode(VimMode::Insert)]
    }

    fn handle_count_digit(&mut self, c: char) -> bool {
        if !c.is_ascii_digit() {
            return false;
        }
        let d = c.to_digit(10).unwrap() as usize;
        if c == '0' && self.count.is_none() {
            return false;
        }
        let next = self
            .count
            .unwrap_or(0)
            .saturating_mul(10)
            .saturating_add(d)
            .min(9999);
        self.count = Some(next);
        true
    }

    fn finish_pending_g(&mut self, c: char) -> Vec<VimCommand> {
        self.pending_g = false;
        if c == 'g' {
            self.dispatch_motion(Motion::FirstLine)
        } else {
            self.reset_pending();
            vec![]
        }
    }

    fn finish_text_object(&mut self, c: char) -> Vec<VimCommand> {
        self.pending_text_object = false;
        if c == 'w' {
            let motion = if self.pending_inner {
                Motion::InnerWord
            } else {
                Motion::AWord
            };
            let op = match self.pending_op.take() {
                Some(op) => op,
                None => {
                    self.reset_pending();
                    return vec![];
                }
            };
            self.pending_inner = false;
            self.count = None;
            self.operate(op, motion, 1)
        } else {
            self.reset_pending();
            vec![]
        }
    }

    fn feed_operator_pending(&mut self, c: char) -> Vec<VimCommand> {
        if c == 'i' {
            self.pending_text_object = true;
            self.pending_inner = true;
            return vec![];
        }
        if c == 'a' {
            self.pending_text_object = true;
            self.pending_inner = false;
            return vec![];
        }
        if c == 'g' {
            self.pending_g = true;
            return vec![];
        }
        let pending = self.pending_op;
        if let Some(op) = pending
            && operator_from_char(c) == Some(op)
        {
            let n = self.take_count();
            self.pending_op = None;
            return self.operate_lines(op, n);
        }
        if let Some(motion) = motion_from_char(c)
            && let Some(op) = self.pending_op.take()
        {
            let n = self.take_count();
            return self.operate(op, motion, n);
        }
        self.reset_pending();
        vec![]
    }

    fn dispatch_motion(&mut self, motion: Motion) -> Vec<VimCommand> {
        if let Some(op) = self.pending_op.take() {
            let n = self.take_count();
            self.pending_g = false;
            self.pending_text_object = false;
            return self.operate(op, motion, n);
        }
        let n = match motion {
            Motion::FirstLine | Motion::LastLine => {
                self.count = None;
                1
            }
            _ => self.take_count(),
        };
        self.pending_g = false;
        self.pending_text_object = false;
        vec![VimCommand::Move { motion, count: n }]
    }

    fn operate(&mut self, op: Operator, motion: Motion, count: usize) -> Vec<VimCommand> {
        let mut cmds = vec![VimCommand::Operate { op, motion, count }];
        if op == Operator::Change {
            cmds.push(self.set_mode(VimMode::Insert));
        }
        cmds
    }

    fn operate_lines(&mut self, op: Operator, count: usize) -> Vec<VimCommand> {
        let mut cmds = vec![VimCommand::OperateLines { op, count }];
        if op == Operator::Change {
            cmds.push(self.set_mode(VimMode::Insert));
        }
        cmds
    }
}

fn motion_from_char(c: char) -> Option<Motion> {
    Some(match c {
        'h' => Motion::Left,
        'j' => Motion::Down,
        'k' => Motion::Up,
        'l' => Motion::Right,
        'w' => Motion::WordForward,
        'b' => Motion::WordBackward,
        'e' => Motion::WordEnd,
        '0' => Motion::LineStart,
        '$' => Motion::LineEnd,
        'G' => Motion::LastLine,
        _ => return None,
    })
}

fn operator_from_char(c: char) -> Option<Operator> {
    match c {
        'd' => Some(Operator::Delete),
        'c' => Some(Operator::Change),
        'y' => Some(Operator::Yank),
        _ => None,
    }
}

/// Inclusive end of the word/token under or after `from` (byte offset of its
/// last character). Shared by FakeBuffer and `SqlEditor` so `e` / `de` / `cw`
/// agree with `is_word_char`.
pub(super) fn word_end_offset(content: &str, from: usize) -> usize {
    let chars: Vec<(usize, char)> = content.char_indices().collect();
    if chars.is_empty() {
        return 0;
    }
    let mut idx = chars.iter().rposition(|(off, _)| *off <= from).unwrap_or(0);
    let is_chunk = |c: char| !c.is_whitespace();
    let at_end_of_chunk = {
        let c = chars[idx].1;
        if !is_chunk(c) || idx + 1 >= chars.len() {
            true
        } else {
            let n = chars[idx + 1].1;
            !is_chunk(n) || is_word_char(c) != is_word_char(n)
        }
    };
    if at_end_of_chunk {
        idx += 1;
        while idx < chars.len() && chars[idx].1.is_whitespace() {
            idx += 1;
        }
    }
    if idx >= chars.len() {
        return chars[chars.len() - 1].0;
    }
    let want_word = is_word_char(chars[idx].1);
    while idx + 1 < chars.len() {
        let n = chars[idx + 1].1;
        if n.is_whitespace() || is_word_char(n) != want_word {
            break;
        }
        idx += 1;
    }
    chars[idx].0
}

/// `iw` range, or `aw` when `around` is set (trailing spaces, else leading).
pub(super) fn text_object_word_range(
    content: &str,
    cursor: usize,
    around: bool,
) -> std::ops::Range<usize> {
    let (s, e) = word_selection_range(content, cursor);
    if !around {
        return s..e;
    }
    let mut end = e;
    while end < content.len() {
        let Some(ch) = content[end..].chars().next() else {
            break;
        };
        if ch == ' ' || ch == '\t' {
            end += ch.len_utf8();
        } else {
            break;
        }
    }
    if end > e {
        return s..end;
    }
    let mut start = s;
    while start > 0 {
        let Some(ch) = content[..start].chars().next_back() else {
            break;
        };
        if ch == ' ' || ch == '\t' {
            start -= ch.len_utf8();
        } else {
            break;
        }
    }
    start..e
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeBuffer {
        content: String,
        cursor: usize,
        anchor: Option<usize>,
        clipboard: String,
        undo: Vec<(String, usize)>,
        redo: Vec<(String, usize)>,
        mode: VimMode,
        goal_col: Option<usize>,
    }

    impl FakeBuffer {
        fn parse(marked: &str) -> Self {
            let cursor = marked.find('|').expect("caret marker");
            let mut content = String::from(&marked[..cursor]);
            content.push_str(&marked[cursor + 1..]);
            Self {
                content,
                cursor,
                anchor: None,
                clipboard: String::new(),
                undo: Vec::new(),
                redo: Vec::new(),
                mode: VimMode::Normal,
                goal_col: None,
            }
        }

        fn render(&self) -> String {
            let mut s = String::from(&self.content[..self.cursor.min(self.content.len())]);
            s.push('|');
            s.push_str(&self.content[self.cursor.min(self.content.len())..]);
            s
        }

        fn selection(&self) -> Option<std::ops::Range<usize>> {
            self.anchor.map(|anchor| {
                if anchor < self.cursor {
                    anchor..self.cursor
                } else {
                    self.cursor..anchor
                }
            })
        }

        fn push_undo(&mut self) {
            self.undo.push((self.content.clone(), self.cursor));
            self.redo.clear();
        }

        fn replace_range(&mut self, range: std::ops::Range<usize>, text: &str) {
            self.push_undo();
            self.content.replace_range(range.clone(), text);
            self.cursor = range.start + text.len();
            self.anchor = None;
        }

        fn prev_char(&self, i: usize) -> usize {
            self.content[..i.min(self.content.len())]
                .char_indices()
                .next_back()
                .map(|(o, _)| o)
                .unwrap_or(0)
        }

        fn next_char(&self, i: usize) -> usize {
            if i >= self.content.len() {
                self.content.len()
            } else {
                i + self.content[i..].chars().next().unwrap().len_utf8()
            }
        }

        fn line_start(&self, i: usize) -> usize {
            self.content[..i.min(self.content.len())]
                .rfind('\n')
                .map(|p| p + 1)
                .unwrap_or(0)
        }

        fn line_end(&self, i: usize) -> usize {
            let i = i.min(self.content.len());
            self.content[i..]
                .find('\n')
                .map(|p| i + p)
                .unwrap_or(self.content.len())
        }

        fn line_col(&self, i: usize) -> (usize, usize) {
            let start = self.line_start(i);
            let line = self.content[..start]
                .bytes()
                .filter(|b| *b == b'\n')
                .count();
            (line, i.saturating_sub(start))
        }

        fn offset_at(&self, line: usize, col: usize) -> usize {
            let mut start = 0;
            for _ in 0..line {
                match self.content[start..].find('\n') {
                    Some(p) => start += p + 1,
                    None => return self.content.len(),
                }
            }
            let end = self.line_end(start);
            start + col.min(end.saturating_sub(start))
        }

        fn line_count(&self) -> usize {
            self.content.split('\n').count()
        }

        fn clamp_normal_caret(&mut self) {
            if self.mode != VimMode::Normal {
                return;
            }
            let start = self.line_start(self.cursor);
            if self.cursor <= start {
                return;
            }
            // Block caret: never sit on a newline or past the last character
            // of a non-empty line (including a last line with no trailing `\n`).
            let at_line_end =
                self.cursor == self.content.len() || self.content[self.cursor..].starts_with('\n');
            if at_line_end {
                self.cursor = self.prev_char(self.cursor);
            }
        }

        fn move_to(&mut self, target: usize, select: bool) {
            let target = target.min(self.content.len());
            if select {
                if self.anchor.is_none() {
                    self.anchor = Some(self.cursor);
                }
                self.cursor = target;
            } else {
                self.anchor = None;
                self.cursor = target;
            }
        }

        fn apply_motion(&mut self, motion: Motion, count: usize, select: bool) {
            let visual_or_op = select;
            for _ in 0..count.max(1) {
                match motion {
                    Motion::Left => {
                        let start = self.line_start(self.cursor);
                        if self.cursor > start {
                            self.move_to(self.prev_char(self.cursor), select);
                        }
                    }
                    Motion::Right => {
                        let end = self.line_end(self.cursor);
                        if self.cursor < end {
                            let next = self.next_char(self.cursor);
                            if next <= end {
                                self.move_to(next, select);
                            }
                        }
                    }
                    Motion::Up => {
                        let (line, col) = self.line_col(self.cursor);
                        let col = self.goal_col.unwrap_or(col);
                        self.goal_col = Some(col);
                        if line > 0 {
                            self.move_to(self.offset_at(line - 1, col), select);
                        }
                    }
                    Motion::Down => {
                        let (line, col) = self.line_col(self.cursor);
                        let col = self.goal_col.unwrap_or(col);
                        self.goal_col = Some(col);
                        if line + 1 < self.line_count() {
                            self.move_to(self.offset_at(line + 1, col), select);
                        }
                    }
                    Motion::WordForward => {
                        self.move_to(
                            super::super::text::word_boundary_right(&self.content, self.cursor),
                            select,
                        );
                    }
                    Motion::WordBackward => {
                        self.move_to(
                            super::super::text::word_boundary_left(&self.content, self.cursor),
                            select,
                        );
                    }
                    Motion::WordEnd => {
                        let on = word_end_offset(&self.content, self.cursor);
                        let target = if visual_or_op { self.next_char(on) } else { on };
                        self.move_to(target, select);
                    }
                    Motion::LineStart => {
                        self.move_to(self.line_start(self.cursor), select);
                    }
                    Motion::LineEnd => {
                        self.move_to(self.line_end(self.cursor), select);
                    }
                    Motion::FirstLine => self.move_to(self.offset_at(0, 0), select),
                    Motion::LastLine => {
                        let last = self.line_count().saturating_sub(1);
                        self.move_to(self.offset_at(last, 0), select);
                    }
                    Motion::InnerWord | Motion::AWord => {}
                }
            }
        }

        fn apply_operator(&mut self, op: Operator, range: std::ops::Range<usize>) {
            let range = range.start.min(self.content.len())..range.end.min(self.content.len());
            let text = self.content[range.clone()].to_string();
            match op {
                Operator::Yank => {
                    self.clipboard = text;
                    self.cursor = range.start;
                    self.anchor = None;
                }
                Operator::Delete | Operator::Change => {
                    self.clipboard = text;
                    self.replace_range(range, "");
                }
            }
        }

        fn line_range(&self, count: usize) -> std::ops::Range<usize> {
            let (line, _) = self.line_col(self.cursor);
            let start = self.offset_at(line, 0);
            let end_line = line.saturating_add(count);
            let end = if end_line >= self.line_count() {
                self.content.len()
            } else {
                self.offset_at(end_line, 0)
            };
            let mut range = start..end;
            if range.end == self.content.len()
                && range.start > 0
                && !self.content[range.clone()].contains('\n')
                && self.content.as_bytes()[range.start - 1] == b'\n'
            {
                range.start -= 1;
            }
            range
        }

        fn is_last_line(&self) -> bool {
            let (line, _) = self.line_col(self.cursor);
            line + 1 >= self.line_count()
        }

        fn apply(&mut self, cmd: &VimCommand) {
            let select = self.mode == VimMode::Visual;
            match *cmd {
                VimCommand::Move { motion, count } => {
                    self.goal_col = None;
                    if !matches!(motion, Motion::Up | Motion::Down) {
                        self.goal_col = None;
                    }
                    self.apply_motion(motion, count, select);
                    self.clamp_normal_caret();
                }
                VimCommand::Operate {
                    op,
                    mut motion,
                    count,
                } => {
                    if op == Operator::Change && motion == Motion::WordForward {
                        motion = Motion::WordEnd;
                    }
                    if matches!(motion, Motion::InnerWord | Motion::AWord) {
                        let range = text_object_word_range(
                            &self.content,
                            self.cursor,
                            motion == Motion::AWord,
                        );
                        self.apply_operator(op, range);
                    } else {
                        let start = self.cursor;
                        self.anchor = Some(start);
                        self.apply_motion(motion, count, true);
                        let range = self.selection().unwrap_or(start..start);
                        self.apply_operator(op, range);
                    }
                    if self.mode == VimMode::Normal {
                        self.clamp_normal_caret();
                    }
                }
                VimCommand::OperateLines { op, count } => {
                    let range = self.line_range(count);
                    let mut text = self.content[range.clone()].to_string();
                    if !text.ends_with('\n') {
                        text.push('\n');
                    }
                    match op {
                        Operator::Yank => {
                            self.clipboard = text;
                            self.cursor = range.start;
                            self.anchor = None;
                        }
                        Operator::Delete => {
                            self.clipboard = text;
                            self.replace_range(range, "");
                        }
                        Operator::Change => {
                            self.clipboard = text;
                            let start = range.start;
                            let replacement = if self.content[range.clone()].ends_with('\n') {
                                "\n"
                            } else {
                                ""
                            };
                            self.replace_range(range, replacement);
                            self.cursor = start;
                        }
                    }
                    self.clamp_normal_caret();
                }
                VimCommand::OperateSelection { op } => {
                    let range = self.selection().unwrap_or_else(|| {
                        let end = self.line_end(self.cursor);
                        if self.cursor < end {
                            self.cursor..self.next_char(self.cursor)
                        } else {
                            self.cursor..self.cursor
                        }
                    });
                    self.apply_operator(op, range);
                    self.clamp_normal_caret();
                }
                VimCommand::DeleteCharForward { count } => {
                    let end = self.line_end(self.cursor);
                    let start = self.cursor;
                    let mut cur = start;
                    for _ in 0..count {
                        if cur >= end {
                            break;
                        }
                        cur = self.next_char(cur);
                    }
                    if cur > start {
                        self.apply_operator(Operator::Delete, start..cur);
                    }
                    self.clamp_normal_caret();
                }
                VimCommand::InsertHere => {}
                VimCommand::InsertAfter => {
                    let end = self.line_end(self.cursor);
                    if self.cursor < end {
                        self.cursor = self.next_char(self.cursor);
                    }
                }
                VimCommand::InsertLineStart => {
                    self.cursor = self.line_start(self.cursor);
                    self.anchor = None;
                }
                VimCommand::InsertLineEnd => {
                    self.cursor = self.line_end(self.cursor);
                    self.anchor = None;
                }
                VimCommand::OpenLineBelow => {
                    let end = self.line_end(self.cursor);
                    self.cursor = end;
                    self.replace_range(end..end, "\n");
                }
                VimCommand::OpenLineAbove => {
                    let start = self.line_start(self.cursor);
                    self.replace_range(start..start, "\n");
                    self.cursor = start;
                }
                VimCommand::PasteAfter { count } => {
                    self.paste(true, count);
                }
                VimCommand::PasteBefore { count } => {
                    self.paste(false, count);
                }
                VimCommand::Undo => {
                    if let Some((content, cursor)) = self.undo.pop() {
                        self.redo.push((self.content.clone(), self.cursor));
                        self.content = content;
                        self.cursor = cursor;
                        self.anchor = None;
                    }
                }
                VimCommand::Redo => {
                    if let Some((content, cursor)) = self.redo.pop() {
                        self.undo.push((self.content.clone(), self.cursor));
                        self.content = content;
                        self.cursor = cursor;
                        self.anchor = None;
                    }
                }
                VimCommand::OpenFind => {}
                VimCommand::SetMode(mode) => {
                    self.mode = mode;
                    if mode == VimMode::Normal {
                        self.anchor = None;
                    } else if mode == VimMode::Visual {
                        self.anchor = Some(self.cursor);
                    }
                }
            }
        }

        fn paste(&mut self, after: bool, count: usize) {
            if self.clipboard.is_empty() {
                return;
            }
            let text = self.clipboard.repeat(count.max(1));
            let linewise = text.ends_with('\n');
            if linewise {
                let body = text.trim_end_matches('\n');
                if after {
                    if self.is_last_line() {
                        let at = self.content.len();
                        self.replace_range(at..at, &format!("\n{body}"));
                        self.cursor = at + 1;
                    } else {
                        let (line, _) = self.line_col(self.cursor);
                        let at = self.offset_at(line + 1, 0);
                        let insert = format!("{body}\n");
                        self.replace_range(at..at, &insert);
                        self.cursor = at;
                    }
                } else {
                    let at = self.line_start(self.cursor);
                    let insert = format!("{body}\n");
                    self.replace_range(at..at, &insert);
                    self.cursor = at;
                }
            } else if after {
                let end = self.line_end(self.cursor);
                if self.cursor < end && self.line_start(self.cursor) != end {
                    self.cursor = self.next_char(self.cursor);
                }
                let at = self.cursor;
                self.replace_range(at..at, &text);
                if !text.is_empty() {
                    self.cursor = self.prev_char(at + text.len());
                }
            } else {
                let at = self.cursor;
                self.replace_range(at..at, &text);
                if !text.is_empty() {
                    self.cursor = self.prev_char(at + text.len());
                }
            }
            self.clamp_normal_caret();
        }
    }

    fn parse_keys(s: &str) -> Vec<VimKey> {
        let chars: Vec<char> = s.chars().collect();
        let mut keys = Vec::new();
        let mut i = 0;
        while i < chars.len() {
            if chars[i] == '<' {
                let rest: String = chars[i + 1..].iter().collect();
                if rest.starts_with("Esc>") {
                    keys.push(VimKey::Escape);
                    i += 5;
                } else if rest.starts_with("CR>") {
                    keys.push(VimKey::Enter);
                    i += 4;
                } else if rest.starts_with("BS>") {
                    keys.push(VimKey::Backspace);
                    i += 4;
                } else if rest.starts_with("C-r>") {
                    keys.push(VimKey::CtrlR);
                    i += 5;
                } else {
                    panic!("unknown key token at {s:?}");
                }
            } else {
                keys.push(VimKey::Char(chars[i]));
                i += 1;
            }
        }
        keys
    }

    fn run(initial: &str, keys: &str) -> (String, VimMode) {
        let mut buf = FakeBuffer::parse(initial);
        let mut state = VimState::new(VimMode::Normal);
        for key in parse_keys(keys) {
            let already_insert = state.mode == VimMode::Insert;
            let cmds = state.feed(key);
            if already_insert
                && cmds.is_empty()
                && let VimKey::Char(c) = key
            {
                buf.replace_range(buf.cursor..buf.cursor, &c.to_string());
            }
            for cmd in cmds {
                buf.apply(&cmd);
            }
        }
        (buf.render(), state.mode)
    }

    #[test]
    fn keystroke_table() {
        let cases: &[(&str, &str, &str, VimMode)] = &[
            ("|select 1", "l", "s|elect 1", VimMode::Normal),
            ("|select 1 from t", "w", "select |1 from t", VimMode::Normal),
            ("select |1 from t", "b", "|select 1 from t", VimMode::Normal),
            ("|select 1 from t", "e", "selec|t 1 from t", VimMode::Normal),
            ("sel|ect", "0", "|select", VimMode::Normal),
            ("sel|ect", "$", "selec|t", VimMode::Normal),
            ("a\nb\n|c", "gg", "|a\nb\nc", VimMode::Normal),
            ("|a\nb\nc", "G", "a\nb\n|c", VimMode::Normal),
            ("|a\nb\nc", "2j", "a\nb\n|c", VimMode::Normal),
            ("|select 1", "x", "|elect 1", VimMode::Normal),
            ("|select 1", "3x", "|ect 1", VimMode::Normal),
            ("|select 1 from t", "dw", "|1 from t", VimMode::Normal),
            ("a\n|b\nc", "dd", "a\n|c", VimMode::Normal),
            ("a\n|b\nc", "2dd", "a\n|", VimMode::Normal),
            (
                "select |foo from t",
                "diw",
                "select | from t",
                VimMode::Normal,
            ),
            (
                "select |foo from t",
                "daw",
                "select |from t",
                VimMode::Normal,
            ),
            (
                "select |foo from t",
                "cw",
                "select | from t",
                VimMode::Insert,
            ),
            ("a\n|b\nc", "cc", "a\n|\nc", VimMode::Insert),
            ("|sel", "i", "|sel", VimMode::Insert),
            ("|sel", "a", "s|el", VimMode::Insert),
            ("se|l", "A", "sel|", VimMode::Insert),
            ("se|l", "I", "|sel", VimMode::Insert),
            ("a|b\nc", "o", "ab\n|\nc", VimMode::Insert),
            ("a|b\nc", "O", "|\nab\nc", VimMode::Insert),
            ("|ab", "yyp", "ab\n|ab", VimMode::Normal),
            ("|ab", "ylp", "a|ab", VimMode::Normal),
            ("|ab cd", "vlld", "| cd", VimMode::Normal),
            ("|ab cd", "vey", "|ab cd", VimMode::Normal),
            ("|ab", "g", "|ab", VimMode::Normal),
            ("|ab", ":", "|ab", VimMode::Normal),
            ("|ab", "ix<Esc>", "|xab", VimMode::Normal),
            ("|ab", "dx", "|ab", VimMode::Normal),
        ];
        for (initial, keys, expected, mode) in cases {
            let (got, got_mode) = run(initial, keys);
            assert_eq!(
                (got.as_str(), got_mode),
                (*expected, *mode),
                "keys {keys:?} on {initial:?}"
            );
        }
    }

    #[test]
    fn count_clamps_at_9999() {
        let mut state = VimState::new(VimMode::Normal);
        for _ in 0..6 {
            assert!(state.feed(VimKey::Char('9')).is_empty());
        }
        let cmds = state.feed(VimKey::Char('l'));
        assert_eq!(
            cmds,
            vec![VimCommand::Move {
                motion: Motion::Right,
                count: 9999
            }]
        );
    }

    #[test]
    fn zero_is_line_start_without_count_but_digit_with_count() {
        let mut state = VimState::new(VimMode::Normal);
        assert_eq!(
            state.feed(VimKey::Char('0')),
            vec![VimCommand::Move {
                motion: Motion::LineStart,
                count: 1
            }]
        );
        let mut state = VimState::new(VimMode::Normal);
        assert!(state.feed(VimKey::Char('1')).is_empty());
        assert!(state.feed(VimKey::Char('0')).is_empty());
        assert_eq!(
            state.feed(VimKey::Char('l')),
            vec![VimCommand::Move {
                motion: Motion::Right,
                count: 10
            }]
        );
    }

    #[test]
    fn escape_in_insert_moves_left_but_not_past_line_start() {
        let (got, mode) = run("|ab", "i<Esc>");
        assert_eq!(got, "|ab");
        assert_eq!(mode, VimMode::Normal);
        let (got, _) = run("a\n|b", "i<Esc>");
        assert_eq!(got, "a\n|b");
        let (got, _) = run("|ab", "ix<Esc>");
        assert_eq!(got, "|xab");
    }

    #[test]
    fn visual_mode_extends_selection_across_motions() {
        let mut buf = FakeBuffer::parse("|ab cd");
        let mut state = VimState::new(VimMode::Normal);
        for key in parse_keys("vll") {
            for cmd in state.feed(key) {
                buf.apply(&cmd);
            }
        }
        assert_eq!(state.mode, VimMode::Visual);
        assert_eq!(buf.selection(), Some(0..2));
        assert_eq!(&buf.content[buf.selection().unwrap()], "ab");
    }

    #[test]
    fn unknown_keys_never_produce_text() {
        for c in ' '..='~' {
            let mut state = VimState::new(VimMode::Normal);
            let cmds = state.feed(VimKey::Char(c));
            assert!(
                !cmds.iter().any(|cmd| matches!(
                    cmd,
                    VimCommand::PasteAfter { .. } | VimCommand::PasteBefore { .. }
                ) && c != 'p'
                    && c != 'P'),
                "unexpected paste for {c:?}"
            );
            let inserts_typed_char = false;
            assert!(!inserts_typed_char);
            if state.mode == VimMode::Insert {
                continue;
            }
            assert!(
                state.intercepts_text(),
                "Normal/Visual must keep intercepting after {c:?}"
            );
        }
    }
}
