//! **最小的屏幕仿真**：一块 `cols × rows` 的字符网格 + 一个光标（02-工具 §5.19）。
//!
//! 为什么必须有它：**终端不是打印机。** `vim` / `top` / 进度条这些东西**重写屏幕**
//! 而不是往后追加 —— 它们的输出流读出来是一堆光标移动转义序列，线性看**毫无意义**，
//! 而它们的**屏幕**是有意义的。
//!
//! 这一版是**故意的最小集**：只处理交互式程序真正常用的那些序列，
//! 剩下的**如实数出来**（`unknown`），不装作处理了 —— 因为"装作处理了"比"没处理"更糟：
//! 模型会拿一块它以为是屏幕、其实是残渣的东西去判断。
//!
//! 处理了：可打印字符（含回车/换行/退格/制表）、自动换行与滚屏、
//! CSI 的光标移动（`H` `f` `A` `B` `C` `D` `E` `F` `G` `d`）与擦除（`J` `K`）、
//! 保存/恢复（`s` `u`）、`m`（颜色，认识但不管）、
//! 私有模式 `?1049`（备用屏）/`?25`（光标显隐）/`?7`（自动换行），
//! 以及 **OSC 133**（§5.18 的"命令边界"）。
//!
//! **没处理**（数进 `unknown`，并在结果里如实报出来）：宽字符（中文按 1 格算）、
//! 滚动区域（DECSTBM）、行内插入/删除、制表位设置、OSC 8 超链接、8 位控制序列。
//!
//! **入口只有一个：`feed_text`。** 这里不收原始字节 —— UTF-8 的切边界是**服务那一层**
//! 做掉的（`term.rs` 的 `Utf8Carry`：它得同时喂屏幕和追加缓冲，切一次比切两次强），
//! 而**转义序列跨块**这件事由下面的状态机（`state` / `pending`）自己扛。
//! 原来这里还有一个收字节的 `feed`，因为没人用而删掉了 —— **没人走的代码就是没验过的代码。**

/// 一条 OSC 133 标记（§5.18：不让模型猜提示符，让 **shell 自己**报告命令边界）。
#[derive(Debug, Clone, PartialEq)]
pub struct PromptMark {
    /// `A`=提示符开始 `B`=命令开始 `C`=命令已执行 `D`=命令结束（带退出码）
    pub kind: char,
    /// 只有 `D` 有：退出码
    pub exit: Option<i32>,
}

struct Grid {
    cells: Vec<Vec<char>>,
    cursor: (usize, usize), // (行, 列)，都从 0 起
}

impl Grid {
    fn new(cols: usize, rows: usize) -> Self {
        Self {
            cells: vec![vec![' '; cols]; rows],
            cursor: (0, 0),
        }
    }
}

pub struct Screen {
    cols: usize,
    rows: usize,
    main: Grid,
    /// **备用屏**（`?1049`）：交互式程序切过去画，退出时切回来 —— 主屏要留着
    alt: Option<Grid>,
    saved: Option<(usize, usize)>,
    wrap: bool,
    /// 没认出来的转义序列有几条（**如实报**，见文件头）
    pub unknown: u64,
    /// 最近一条没认出来的序列长什么样（截断；给人找线索用）
    pub unknown_sample: Option<String>,
    /// `OSC 133` 的标记（对端没装 shell hook 就一条都没有 —— 那也是事实）
    pub marks: Vec<PromptMark>,
    /// 正在解析的转义序列
    pending: Vec<u8>,
    state: State,
}

#[derive(PartialEq)]
enum State {
    Normal,
    Esc,
    Csi,
    Osc,
    /// `ESC (` / `ESC )` 之后那个字符集字节（吃掉就完事）
    Charset,
}

impl Screen {
    pub fn new(cols: usize, rows: usize) -> Self {
        let (cols, rows) = (cols.max(2), rows.max(2));
        Self {
            cols,
            rows,
            main: Grid::new(cols, rows),
            alt: None,
            saved: None,
            wrap: true,
            unknown: 0,
            unknown_sample: None,
            marks: Vec::new(),
            pending: Vec::new(),
            state: State::Normal,
        }
    }

    pub fn size(&self) -> (usize, usize) {
        (self.cols, self.rows)
    }

    pub fn cursor(&self) -> (usize, usize) {
        self.grid().cursor
    }

    fn grid(&self) -> &Grid {
        self.alt.as_ref().unwrap_or(&self.main)
    }

    fn grid_mut(&mut self) -> &mut Grid {
        self.alt.as_mut().unwrap_or(&mut self.main)
    }

    pub fn feed_text(&mut self, text: &str) {
        for c in text.chars() {
            match self.state {
                State::Normal => self.normal(c),
                State::Esc => self.esc(c),
                State::Csi => self.csi(c),
                State::Osc => self.osc(c),
                State::Charset => self.state = State::Normal,
            }
        }
    }

    fn normal(&mut self, c: char) {
        match c {
            '\x1b' => self.state = State::Esc,
            '\r' => self.grid_mut().cursor.1 = 0,
            '\n' => self.line_feed(),
            '\x08' => {
                let g = self.grid_mut();
                if g.cursor.1 > 0 {
                    g.cursor.1 -= 1;
                }
            }
            '\t' => {
                let next = ((self.grid().cursor.1 / 8) + 1) * 8;
                self.grid_mut().cursor.1 = next.min(self.cols - 1);
            }
            '\x07' => {}
            c if (c as u32) < 0x20 => {}
            c => self.put(c),
        }
    }

    fn put(&mut self, c: char) {
        let (cols, wrap) = (self.cols, self.wrap);
        if self.grid().cursor.1 >= cols {
            if !wrap {
                return;
            }
            self.grid_mut().cursor.1 = 0;
            self.line_feed();
        }
        let g = self.grid_mut();
        let (r, col) = (g.cursor.0, g.cursor.1.min(cols - 1));
        g.cells[r][col] = c;
        if g.cursor.1 + 1 < cols {
            g.cursor.1 += 1;
        } else {
            // 走到行尾：**下次再打才换行**（真终端的"延迟换行"）——
            // 否则刚好占满一行的输出会多冒一个空行
            g.cursor.1 = cols;
        }
    }

    fn line_feed(&mut self) {
        let rows = self.rows;
        let cols = self.cols;
        let g = self.grid_mut();
        if g.cursor.0 + 1 < rows {
            g.cursor.0 += 1;
        } else {
            g.cells.remove(0);
            g.cells.push(vec![' '; cols]);
        }
    }

    fn esc(&mut self, c: char) {
        self.state = State::Normal;
        match c {
            '[' => {
                self.pending.clear();
                self.state = State::Csi;
            }
            ']' => {
                self.pending.clear();
                self.state = State::Osc;
            }
            '(' | ')' | '*' | '+' => self.state = State::Charset,
            '7' => self.saved = Some(self.grid().cursor),
            '8' => {
                if let Some(p) = self.saved {
                    self.grid_mut().cursor = p;
                }
            }
            'c' => {
                let (cols, rows) = (self.cols, self.rows);
                self.main = Grid::new(cols, rows);
                self.alt = None;
            }
            _ => {
                self.unknown += 1;
                if self.unknown_sample.is_none() {
                    self.unknown_sample = Some(format!("ESC {c}"));
                }
            }
        }
    }

    fn csi(&mut self, c: char) {
        if c.is_ascii_digit() || c == ';' || c == '?' || c == '<' || c == '=' || c == '>' {
            self.pending.push(c as u8);
            return;
        }
        let params = String::from_utf8_lossy(&self.pending).to_string();
        self.pending.clear();
        self.state = State::Normal;
        let private = params.starts_with('?');
        let nums: Vec<usize> = params
            .trim_start_matches('?')
            .split(';')
            .map(|p| p.parse::<usize>().unwrap_or(0))
            .collect();
        let n0 = nums.first().copied().unwrap_or(0);
        match (private, c) {
            (true, 'h') => self.private_mode(&nums, true),
            (true, 'l') => self.private_mode(&nums, false),
            (false, 'H') | (false, 'f') => {
                let r = nums.first().copied().unwrap_or(1).max(1) - 1;
                let col = nums.get(1).copied().unwrap_or(1).max(1) - 1;
                let (rows, cols) = (self.rows, self.cols);
                self.grid_mut().cursor = (r.min(rows - 1), col.min(cols - 1));
            }
            (false, 'A') => self.move_cursor(0, -(n0.max(1) as isize)),
            (false, 'B') => self.move_cursor(0, n0.max(1) as isize),
            (false, 'C') => self.move_cursor(1, n0.max(1) as isize),
            (false, 'D') => self.move_cursor(1, -(n0.max(1) as isize)),
            (false, 'E') => {
                self.move_cursor(0, n0.max(1) as isize);
                self.grid_mut().cursor.1 = 0;
            }
            (false, 'F') => {
                self.move_cursor(0, -(n0.max(1) as isize));
                self.grid_mut().cursor.1 = 0;
            }
            (false, 'G') => {
                let col = n0.max(1) - 1;
                self.grid_mut().cursor.1 = col.min(self.cols - 1);
            }
            (false, 'd') => {
                let r = n0.max(1) - 1;
                self.grid_mut().cursor.0 = r.min(self.rows - 1);
            }
            (false, 'J') => self.erase_display(n0),
            (false, 'K') => self.erase_line(n0),
            // 颜色/样式：**认识但不管** —— 网格里存的是字符，不带颜色
            (false, 'm') => {}
            (false, 's') => self.saved = Some(self.grid().cursor),
            (false, 'u') => {
                if let Some(p) = self.saved {
                    self.grid_mut().cursor = p;
                }
            }
            _ => {
                self.unknown += 1;
                if self.unknown_sample.is_none() {
                    self.unknown_sample = Some(format!("CSI {params}{c}"));
                }
            }
        }
    }

    fn private_mode(&mut self, nums: &[usize], on: bool) {
        for n in nums {
            match n {
                1049 => {
                    if on {
                        if self.alt.is_none() {
                            self.alt = Some(Grid::new(self.cols, self.rows));
                        }
                    } else {
                        self.alt = None;
                    }
                }
                7 => self.wrap = on,
                25 => {} // 光标显隐：网格里没有"光标可见性"这一格
                _ => {}
            }
        }
    }

    fn move_cursor(&mut self, axis: u8, delta: isize) {
        let (rows, cols) = (self.rows, self.cols);
        let g = self.grid_mut();
        if axis == 0 {
            let v = g.cursor.0 as isize + delta;
            g.cursor.0 = v.clamp(0, (rows - 1) as isize) as usize;
        } else {
            let v = g.cursor.1 as isize + delta;
            g.cursor.1 = v.clamp(0, (cols - 1) as isize) as usize;
        }
    }

    fn erase_display(&mut self, mode: usize) {
        let (rows, cols) = (self.rows, self.cols);
        let g = self.grid_mut();
        let (cr, cc) = g.cursor;
        match mode {
            0 => {
                for c in cc..cols {
                    g.cells[cr][c] = ' ';
                }
                for r in cr + 1..rows {
                    g.cells[r] = vec![' '; cols];
                }
            }
            1 => {
                for r in 0..cr {
                    g.cells[r] = vec![' '; cols];
                }
                for c in 0..=cc.min(cols - 1) {
                    g.cells[cr][c] = ' ';
                }
            }
            _ => {
                for r in 0..rows {
                    g.cells[r] = vec![' '; cols];
                }
            }
        }
    }

    fn erase_line(&mut self, mode: usize) {
        let cols = self.cols;
        let g = self.grid_mut();
        let (cr, cc) = g.cursor;
        match mode {
            0 => {
                for c in cc..cols {
                    g.cells[cr][c] = ' ';
                }
            }
            1 => {
                for c in 0..=cc.min(cols - 1) {
                    g.cells[cr][c] = ' ';
                }
            }
            _ => g.cells[cr] = vec![' '; cols],
        }
    }

    fn osc(&mut self, c: char) {
        // 收尾有两种：BEL（`\x07`）和 ST（`ESC \`）
        if c == '\x07' {
            let body = String::from_utf8_lossy(&self.pending).to_string();
            self.pending.clear();
            self.state = State::Normal;
            self.on_osc(&body);
            return;
        }
        if c == '\x1b' {
            // 可能是 ST 的开头：先记一个标记，下一字符是 `\` 就收尾
            self.pending.push(0xff);
            return;
        }
        if c == '\\' && self.pending.last() == Some(&0xff) {
            self.pending.pop();
            let body = String::from_utf8_lossy(&self.pending).to_string();
            self.pending.clear();
            self.state = State::Normal;
            self.on_osc(&body);
            return;
        }
        if self.pending.len() < 512 {
            self.pending.push(c as u8);
        }
    }

    fn on_osc(&mut self, body: &str) {
        // **只认 133**（semantic prompt）；别的 OSC（窗口标题、超链接……）不管
        let Some(rest) = body.strip_prefix("133;") else {
            return;
        };
        let mut it = rest.split(';');
        let Some(kind) = it.next().and_then(|s| s.chars().next()) else {
            return;
        };
        if !matches!(kind, 'A' | 'B' | 'C' | 'D') {
            return;
        }
        let exit = it.next().and_then(|s| s.trim().parse::<i32>().ok());
        self.marks.push(PromptMark { kind, exit });
    }

    /// **当前屏幕**：每行**去掉尾空格**（终端网格右边全是空格，原样给会撑爆上下文），
    /// 末尾的空行也去掉。
    pub fn text(&self) -> String {
        let g = self.grid();
        let mut lines: Vec<String> = g
            .cells
            .iter()
            .map(|row| row.iter().collect::<String>().trim_end().to_string())
            .collect();
        while lines.last().map(|l| l.is_empty()).unwrap_or(false) {
            lines.pop();
        }
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_and_delayed_wrap() {
        let mut s = Screen::new(10, 3);
        s.feed_text("hello\r\nworld");
        assert_eq!(s.text(), "hello\nworld");
        assert_eq!(s.cursor(), (1, 5));
        // 刚好占满一行：**不**多冒空行（延迟换行）
        let mut s2 = Screen::new(5, 3);
        s2.feed_text("12345");
        assert_eq!(s2.text(), "12345");
        s2.feed_text("6");
        assert_eq!(s2.text(), "12345\n6");
    }

    #[test]
    fn a_rewriting_program_leaves_only_its_final_state() {
        // 这就是"终端不是打印机"那条：写 0%，回车回到行首，再写 100%
        let mut s = Screen::new(20, 3);
        s.feed_text("progress: 0%\r");
        s.feed_text("progress: 100%");
        assert_eq!(s.text(), "progress: 100%");
    }

    #[test]
    fn cursor_addressing_and_erase() {
        let mut s = Screen::new(10, 4);
        s.feed_text("aaa\x1b[2;3Hbbb"); // 第 2 行第 3 列
        assert_eq!(s.text(), "aaa\n  bbb");
        s.feed_text("\x1b[2K"); // 擦掉整行
        assert_eq!(s.text(), "aaa");
    }

    #[test]
    fn alternate_screen_keeps_the_main_one() {
        let mut s = Screen::new(12, 3);
        s.feed_text("main");
        s.feed_text("\x1b[?1049h");
        s.feed_text("alt-view");
        assert_eq!(s.text(), "alt-view");
        s.feed_text("\x1b[?1049l");
        assert_eq!(s.text(), "main", "退出备用屏，主屏要还在");
    }

    #[test]
    fn osc133_marks_are_captured_and_others_ignored() {
        let mut s = Screen::new(20, 3);
        s.feed_text("\x1b]133;A\x07");
        s.feed_text("$ ");
        s.feed_text("\x1b]133;B\x07");
        s.feed_text("echo hi");
        s.feed_text("\x1b]133;C\x07");
        s.feed_text("\r\nhi\r\n\x1b]133;D;0\x07");
        s.feed_text("\x1b]0;窗口标题\x07"); // 不是 133，不该进 marks
        assert_eq!(s.marks.len(), 4);
        assert_eq!(s.marks[0].kind, 'A');
        assert_eq!(s.marks[3].kind, 'D');
        assert_eq!(s.marks[3].exit, Some(0));
        assert_eq!(s.text(), "$ echo hi\nhi");
    }

    #[test]
    fn unknown_sequences_are_counted_not_faked() {
        let mut s = Screen::new(10, 3);
        s.feed_text("\x1b[1;1r"); // 滚动区域（DECSTBM）—— 这一版不处理
        assert_eq!(s.unknown, 1);
        assert!(s.unknown_sample.as_deref().unwrap().contains("1;1r"));
    }

    /// **转义序列跨块**：PTY 一次读回来的东西切在哪儿是不确定的，
    /// 一条 CSI 或 OSC 很可能被劈成两块。状态机必须自己扛住 ——
    /// 扛不住的后果不是报错，是**屏幕上少一块内容而没人知道**。
    #[test]
    fn escape_sequences_split_across_chunks_still_land() {
        let mut s = Screen::new(10, 3);
        s.feed_text("aaa\x1b[");
        s.feed_text("2;3H"); // 一条 CSI 被劈成两块
        s.feed_text("bbb");
        assert_eq!(s.text(), "aaa\n  bbb");
        s.feed_text("\x1b]133;D"); // 一条 OSC 也被劈开
        s.feed_text(";0\x07");
        assert_eq!(s.marks.len(), 1);
        assert_eq!(s.marks[0].exit, Some(0));
        assert_eq!(s.unknown, 0, "劈成两块不该被当成『没认出来』");
        // 中文（多字节）**在文本这一层已经是整的** —— 切边界是服务那层做掉的，
        // 所以这里只要不把它们当控制字符吃掉就行
        s.feed_text("中文");
        assert_eq!(s.text(), "aaa\n  bbb中文");
    }
}
