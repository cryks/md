//! alternate screen 上の表示状態、キー操作、ファイル再読み込みを管理する。
//! watch 中に読み込みが失敗した場合は最後に描画できた内容を保持し、同じパスを
//! 次のポーリング周期で読み直す。同じ内容を連続して読めた時点で表示へ反映し、
//! 内容が変わり続ける場合は1秒後に最新候補を反映する。

use std::{
    fs,
    io::{self, Write},
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::Result;
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute, queue,
    style::{Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor},
    terminal::{
        self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
        enable_raw_mode,
    },
};

use crate::{
    renderer,
    style::{
        StyledLine, TextStyle, char_width, display_width, line_with_search_highlight, slice_line,
    },
};

const WATCH_POLL_INTERVAL: Duration = Duration::from_millis(250);
const WATCH_MAX_SETTLE_TIME: Duration = Duration::from_secs(1);

/// Markdown を pager に表示し、終了キーが押されるまで端末セッションを所有する。
///
/// `watch` が有効な間は `path` を 250ms ごとに UTF-8 として読み直す。読み込みに
/// 失敗しても表示中の内容は破棄せず、ステータス行に失敗状態を出して再試行する。
pub(crate) fn run(path: PathBuf, source: String, watch: bool) -> Result<()> {
    let mut terminal = TerminalSession::enter()?;
    let (width, height) = terminal::size()?;
    let mut app = App::new(path, source, width, height, watch);
    app.draw(terminal.writer())?;
    let mut last_watch_check = Instant::now();

    loop {
        let mut should_draw = false;
        let was_watching = app.watch;
        let poll_timeout = if app.watch {
            WATCH_POLL_INTERVAL.saturating_sub(last_watch_check.elapsed())
        } else {
            WATCH_POLL_INTERVAL
        };
        if event::poll(poll_timeout)? {
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    if app.handle_key(key) {
                        break;
                    }
                    should_draw = true;
                }
                Event::Resize(width, height) => {
                    app.resize(width, height);
                    should_draw = true;
                }
                _ => {}
            }
        }

        if !was_watching && app.watch {
            // 無効中の変更を待たせないため、再有効化したループ内で最初の読み込みを行う。
            last_watch_check = Instant::now()
                .checked_sub(WATCH_POLL_INTERVAL)
                .unwrap_or_else(Instant::now);
        }

        if app.watch && last_watch_check.elapsed() >= WATCH_POLL_INTERVAL {
            if app.reload_if_changed() {
                should_draw = true;
            }
            last_watch_check = Instant::now();
        }

        if should_draw {
            app.draw(terminal.writer())?;
        }
    }

    Ok(())
}

struct TerminalSession {
    stdout: io::Stdout,
}

impl TerminalSession {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, Hide)?;
        Ok(Self { stdout })
    }

    fn writer(&mut self) -> &mut io::Stdout {
        &mut self.stdout
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = execute!(self.stdout, Show, LeaveAlternateScreen, ResetColor);
        let _ = disable_raw_mode();
    }
}

#[derive(Debug)]
enum Mode {
    Normal,
    SearchInput(String),
}

struct PendingSource {
    /// 確定前に読めた最新の内容。連続更新中はポーリングごとに置き換える。
    source: String,
    /// 最初の未確定変更を読んだ時刻。内容が変わり続けても更新せず、確定期限に使う。
    first_seen: Instant,
}

struct App {
    path: PathBuf,
    source: String,
    lines: Vec<StyledLine>,
    width: u16,
    height: u16,
    top: usize,
    left: usize,
    query: String,
    matches: Vec<usize>,
    match_index: Option<usize>,
    mode: Mode,
    /// `true` の間だけ `path` を周期的に読み直す。通常モードの `w` で反転する。
    watch: bool,
    /// watch 中の直近の読み込み失敗。成功時または watch の無効化時に `None` へ戻る。
    watch_error: Option<String>,
    /// 安定待ち中の内容と開始時刻。確定、読み込み失敗、watch 無効化で破棄する。
    pending_source: Option<PendingSource>,
}

impl App {
    fn new(path: PathBuf, source: String, width: u16, height: u16, watch: bool) -> Self {
        let lines = renderer::render_markdown(&source, width as usize);
        Self {
            path,
            source,
            lines,
            width,
            height,
            top: 0,
            left: 0,
            query: String::new(),
            matches: Vec::new(),
            match_index: None,
            mode: Mode::Normal,
            watch,
            watch_error: None,
            pending_source: None,
        }
    }

    fn resize(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
        self.lines = renderer::render_markdown(&self.source, width as usize);
        self.rebuild_matches();
        self.clamp_top();
        self.clamp_left();
    }

    fn handle_key(&mut self, key: KeyEvent) -> bool {
        match &mut self.mode {
            Mode::SearchInput(input) => match key.code {
                KeyCode::Esc => {
                    self.mode = Mode::Normal;
                }
                KeyCode::Enter => {
                    self.query = input.clone();
                    self.mode = Mode::Normal;
                    self.rebuild_matches();
                    self.jump_to_match_after_top();
                }
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char(ch) => {
                    input.push(ch);
                }
                _ => {}
            },
            Mode::Normal => match key.code {
                KeyCode::Char('q') => return true,
                KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('e') => self.scroll_lines(1),
                KeyCode::Up | KeyCode::Char('k') | KeyCode::Char('y') => self.scroll_lines(-1),
                KeyCode::PageDown | KeyCode::Char('f') => self.scroll_pages(1),
                KeyCode::PageUp | KeyCode::Char('b') => self.scroll_pages(-1),
                KeyCode::Char('g') => self.top = 0,
                KeyCode::Char('G') => self.top = self.max_top(),
                KeyCode::Char('0') => self.left = 0,
                KeyCode::Char('h') | KeyCode::Left
                    if key.modifiers.contains(KeyModifiers::SHIFT) =>
                {
                    self.scroll_columns(-(self.large_column_scroll() as isize))
                }
                KeyCode::Char('l') | KeyCode::Right
                    if key.modifiers.contains(KeyModifiers::SHIFT) =>
                {
                    self.scroll_columns(self.large_column_scroll() as isize)
                }
                KeyCode::Char('h') | KeyCode::Left => self.scroll_columns(-4),
                KeyCode::Char('l') | KeyCode::Right => self.scroll_columns(4),
                KeyCode::Char('H') => self.scroll_columns(-(self.large_column_scroll() as isize)),
                KeyCode::Char('L') => self.scroll_columns(self.large_column_scroll() as isize),
                KeyCode::Char('/') => self.mode = Mode::SearchInput(String::new()),
                KeyCode::Char('n') => self.next_match(),
                KeyCode::Char('N') => self.previous_match(),
                KeyCode::Char('w')
                    if key.kind == KeyEventKind::Press && key.modifiers == KeyModifiers::NONE =>
                {
                    self.watch = !self.watch;
                    self.watch_error = None;
                    self.pending_source = None;
                }
                _ => {}
            },
        }

        false
    }

    fn draw(&mut self, stdout: &mut io::Stdout) -> Result<()> {
        self.clamp_top();
        self.clamp_left();
        let body_height = self.body_height();

        queue!(stdout, Clear(ClearType::All))?;
        for row in 0..body_height {
            let line_index = self.top + row as usize;
            queue!(stdout, MoveTo(0, row))?;
            if let Some(line) = self.lines.get(line_index) {
                let highlighted = line_with_search_highlight(line, &self.query);
                let visible = slice_line(&highlighted, self.left, self.width as usize);
                draw_line(stdout, &visible)?;
            }
        }

        self.draw_status(stdout)?;
        stdout.flush()?;
        Ok(())
    }

    fn draw_status(&self, stdout: &mut io::Stdout) -> Result<()> {
        let row = self.height.saturating_sub(1);
        let status = match &self.mode {
            Mode::SearchInput(input) => format!("/{input}"),
            Mode::Normal => self.status_text(),
        };
        let text = status_line(status, self.width as usize);

        queue!(
            stdout,
            MoveTo(0, row),
            SetAttribute(crossterm::style::Attribute::Reverse),
            Print(text),
            SetAttribute(crossterm::style::Attribute::Reset),
            ResetColor
        )?;
        Ok(())
    }

    fn status_text(&self) -> String {
        let filename = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("markdown");
        let percent = if self.lines.is_empty() {
            100
        } else {
            ((self.top + self.body_height() as usize).min(self.lines.len()) * 100)
                / self.lines.len()
        };
        let query = if self.query.is_empty() {
            String::new()
        } else {
            format!("  /{}", self.query)
        };
        let column = if self.left == 0 {
            String::new()
        } else {
            format!("  col {}", self.left + 1)
        };

        let watch = match &self.watch_error {
            Some(error) => format!("watch:error ({error})"),
            None if self.watch => "watch:on".to_owned(),
            None => "watch:off".to_owned(),
        };

        format!("{filename}  {percent:>3}%{query}{column}  {watch}  q:quit  /:search  w:watch")
    }

    /// watch が有効ならファイルを読み直し、画面に見える状態が変わったかを返す。
    ///
    /// 新しい内容は `stage_source` の安定条件を満たしてから renderer へ渡す。
    /// 読み込み失敗は `watch_error` のみを更新し、最後に確定した表示を保持する。
    fn reload_if_changed(&mut self) -> bool {
        if !self.watch {
            return false;
        }

        match fs::read_to_string(&self.path) {
            Ok(source) => {
                let recovered = self.watch_error.take().is_some();
                self.stage_source(source, Instant::now()) || recovered
            }
            Err(error) => {
                self.pending_source = None;
                let error = error.to_string();
                if self.watch_error.as_deref() == Some(&error) {
                    return false;
                }
                self.watch_error = Some(error);
                true
            }
        }
    }

    /// 読み取れた内容を安定待ちへ入れ、表示へ確定したかを返す。
    ///
    /// 同じ内容を二回続けて読めた場合は直ちに確定する。内容が変わり続ける場合も
    /// 最初の変更から1秒で最新候補を確定し、生成中のファイルで表示が止まるのを防ぐ。
    fn stage_source(&mut self, source: String, now: Instant) -> bool {
        if source == self.source {
            self.pending_source = None;
            return false;
        }

        let should_commit = self.pending_source.as_ref().is_some_and(|pending| {
            pending.source == source
                || now.saturating_duration_since(pending.first_seen) >= WATCH_MAX_SETTLE_TIME
        });
        if should_commit {
            self.pending_source = None;
            return self.replace_source(source);
        }

        match &mut self.pending_source {
            Some(pending) => pending.source = source,
            None => {
                self.pending_source = Some(PendingSource {
                    source,
                    first_seen: now,
                });
            }
        }
        false
    }

    /// 読み込み済みの内容を置き換え、renderer の入力が変わったかを返す。
    ///
    /// 検索語とスクロール位置は引き継ぐ。新しい行数・表示幅から外れた位置だけを
    /// 末尾へ戻し、検索対象行は新しい描画結果から作り直す。
    fn replace_source(&mut self, source: String) -> bool {
        if source == self.source {
            return false;
        }

        self.source = source;
        self.lines = renderer::render_markdown(&self.source, self.width as usize);
        self.rebuild_matches();
        self.clamp_top();
        self.clamp_left();
        true
    }

    fn scroll_lines(&mut self, delta: isize) {
        self.top = self.top.saturating_add_signed(delta).min(self.max_top());
    }

    fn scroll_pages(&mut self, delta: isize) {
        let amount = self.body_height().max(1) as isize;
        self.scroll_lines(delta * amount);
    }

    fn scroll_columns(&mut self, delta: isize) {
        self.left = self.left.saturating_add_signed(delta).min(self.max_left());
    }

    fn large_column_scroll(&self) -> usize {
        (self.width as usize / 2).max(1)
    }

    fn body_height(&self) -> u16 {
        self.height.saturating_sub(1).max(1)
    }

    fn max_top(&self) -> usize {
        self.lines.len().saturating_sub(self.body_height() as usize)
    }

    fn clamp_top(&mut self) {
        self.top = self.top.min(self.max_top());
    }

    fn max_left(&self) -> usize {
        self.lines
            .iter()
            .map(|line| display_width(&line.plain_text()).saturating_sub(self.width as usize))
            .max()
            .unwrap_or(0)
    }

    fn clamp_left(&mut self) {
        self.left = self.left.min(self.max_left());
    }

    fn rebuild_matches(&mut self) {
        let query = self.query.to_lowercase();
        if query.is_empty() {
            self.matches.clear();
            self.match_index = None;
            return;
        }

        self.matches = self
            .lines
            .iter()
            .enumerate()
            .filter_map(|(index, line)| {
                line.plain_text()
                    .to_lowercase()
                    .contains(&query)
                    .then_some(index)
            })
            .collect();
        self.match_index = None;
    }

    fn jump_to_match_after_top(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        let index = self
            .matches
            .iter()
            .position(|line| *line >= self.top)
            .unwrap_or(0);
        self.match_index = Some(index);
        self.top = self.matches[index].min(self.max_top());
        self.left = 0;
    }

    fn next_match(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        let next = self
            .match_index
            .map(|index| (index + 1) % self.matches.len())
            .unwrap_or(0);
        self.match_index = Some(next);
        self.top = self.matches[next].min(self.max_top());
        self.left = 0;
    }

    fn previous_match(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        let previous = self
            .match_index
            .map(|index| {
                if index == 0 {
                    self.matches.len() - 1
                } else {
                    index - 1
                }
            })
            .unwrap_or_else(|| self.matches.len() - 1);
        self.match_index = Some(previous);
        self.top = self.matches[previous].min(self.max_top());
        self.left = 0;
    }
}

fn draw_line(stdout: &mut io::Stdout, line: &StyledLine) -> Result<()> {
    for span in &line.spans {
        apply_style(stdout, span.style)?;
        queue!(stdout, Print(&span.text))?;
    }
    queue!(
        stdout,
        SetAttribute(crossterm::style::Attribute::Reset),
        ResetColor
    )?;
    Ok(())
}

fn apply_style(stdout: &mut io::Stdout, style: TextStyle) -> Result<()> {
    queue!(
        stdout,
        SetAttribute(crossterm::style::Attribute::Reset),
        ResetColor
    )?;
    if let Some(fg) = style.fg {
        queue!(stdout, SetForegroundColor(fg))?;
    }
    if let Some(bg) = style.bg {
        queue!(stdout, SetBackgroundColor(bg))?;
    }
    for attribute in style.attributes() {
        queue!(stdout, SetAttribute(attribute))?;
    }
    Ok(())
}

fn status_line(text: String, width: usize) -> String {
    let mut output = String::new();
    let mut used = 0usize;

    for ch in text.chars() {
        let ch_width = char_width(ch);
        if used + ch_width > width {
            break;
        }
        output.push(ch);
        used += ch_width;
    }

    output.push_str(&" ".repeat(width.saturating_sub(used)));
    output
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
    };

    use crossterm::event::{KeyEventState, KeyModifiers};

    use super::*;

    static NEXT_TEST_FILE_ID: AtomicU64 = AtomicU64::new(0);

    struct TestFile {
        path: PathBuf,
    }

    impl TestFile {
        fn new(contents: &str) -> Self {
            let id = NEXT_TEST_FILE_ID.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("md-watch-{}-{id}.md", std::process::id()));
            let file = Self { path };
            fs::write(&file.path, contents).unwrap();
            file
        }
    }

    impl Drop for TestFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
        }
    }

    fn app(source: &str, watch: bool) -> App {
        App::new(PathBuf::from("notes.md"), source.to_owned(), 80, 24, watch)
    }

    fn key(code: KeyCode, kind: KeyEventKind, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn toggles_watch_only_on_plain_w_press() {
        let mut app = app("before", false);

        app.handle_key(key(
            KeyCode::Char('w'),
            KeyEventKind::Press,
            KeyModifiers::NONE,
        ));
        assert!(app.watch);

        app.handle_key(key(
            KeyCode::Char('w'),
            KeyEventKind::Repeat,
            KeyModifiers::NONE,
        ));
        assert!(app.watch);

        app.handle_key(key(
            KeyCode::Char('w'),
            KeyEventKind::Release,
            KeyModifiers::NONE,
        ));
        assert!(app.watch);

        app.handle_key(key(
            KeyCode::Char('w'),
            KeyEventKind::Press,
            KeyModifiers::ALT,
        ));
        assert!(app.watch);

        app.handle_key(key(
            KeyCode::Char('w'),
            KeyEventKind::Press,
            KeyModifiers::NONE,
        ));
        assert!(!app.watch);
    }

    #[test]
    fn keeps_w_as_search_input() {
        let mut app = app("before", false);
        app.mode = Mode::SearchInput(String::new());

        app.handle_key(key(
            KeyCode::Char('w'),
            KeyEventKind::Press,
            KeyModifiers::NONE,
        ));

        assert!(!app.watch);
        assert!(matches!(&app.mode, Mode::SearchInput(input) if input == "w"));
    }

    #[test]
    fn reloads_changed_file_and_recovers_after_read_error() {
        let file = TestFile::new("after");
        let mut app = App::new(file.path.clone(), "before".to_owned(), 80, 24, true);

        assert!(!app.reload_if_changed());
        assert!(app.reload_if_changed());
        assert_eq!(app.source, "after");
        assert!(!app.reload_if_changed());

        app.query = "kept query".to_owned();
        app.top = 7;
        app.left = 9;
        let rendered = app
            .lines
            .iter()
            .map(StyledLine::plain_text)
            .collect::<Vec<_>>();
        fs::remove_file(&file.path).unwrap();
        assert!(app.reload_if_changed());
        assert!(app.watch_error.is_some());
        assert_eq!(app.source, "after");
        assert_eq!(app.query, "kept query");
        assert_eq!(app.top, 7);
        assert_eq!(app.left, 9);
        assert_eq!(
            app.lines
                .iter()
                .map(StyledLine::plain_text)
                .collect::<Vec<_>>(),
            rendered
        );
        assert!(!app.reload_if_changed());

        fs::write(&file.path, "after").unwrap();
        assert!(app.reload_if_changed());
        assert!(app.watch_error.is_none());

        fs::write(&file.path, "recovered").unwrap();
        assert!(!app.reload_if_changed());
        assert!(app.reload_if_changed());
        assert_eq!(app.source, "recovered");
    }

    #[test]
    fn replacing_source_rebuilds_search_and_clamps_viewport() {
        let mut app = app("needle\n\nmore", false);
        app.query = "needle".to_owned();
        app.rebuild_matches();
        app.match_index = Some(0);
        app.top = usize::MAX;
        app.left = usize::MAX;

        assert!(app.replace_source("needle moved".to_owned()));
        assert_eq!(app.query, "needle");
        assert_eq!(app.matches, vec![0]);
        assert_eq!(app.match_index, None);
        assert_eq!(app.top, app.max_top());
        assert_eq!(app.left, app.max_left());

        app.match_index = Some(0);
        assert!(!app.replace_source("needle moved".to_owned()));
        assert_eq!(app.match_index, Some(0));
    }

    #[test]
    fn ignores_transient_content_until_a_snapshot_is_stable() {
        let code = (0..40)
            .map(|index| format!("{index}: {}", "x".repeat(40)))
            .collect::<Vec<_>>()
            .join("\n");
        let initial = format!("```text\n{code}\n```");
        let final_source = initial.replace('x', "y");
        let file = TestFile::new(&initial);
        let mut app = App::new(file.path.clone(), initial.clone(), 20, 4, true);
        app.top = 10;
        app.left = 5;

        fs::write(&file.path, "").unwrap();
        assert!(!app.reload_if_changed());
        assert_eq!(app.source, initial);

        fs::write(&file.path, &final_source).unwrap();
        assert!(!app.reload_if_changed());
        assert_eq!(app.top, 10);
        assert_eq!(app.left, 5);

        assert!(app.reload_if_changed());
        assert_eq!(app.source, final_source);
        assert_eq!(app.top, 10);
        assert_eq!(app.left, 5);
    }

    #[test]
    fn disabled_watch_does_not_read_and_status_reports_each_state() {
        let file = TestFile::new("on disk");
        let mut app = App::new(file.path.clone(), "in memory".to_owned(), 80, 24, false);

        assert!(!app.reload_if_changed());
        assert_eq!(app.source, "in memory");
        assert!(app.status_text().contains("watch:off"));
        assert!(app.status_text().contains("w:watch"));

        app.watch = true;
        assert!(app.status_text().contains("watch:on"));
        app.watch_error = Some("read failed".to_owned());
        app.pending_source = Some(PendingSource {
            source: "pending".to_owned(),
            first_seen: Instant::now(),
        });
        assert!(app.status_text().contains("watch:error"));

        app.handle_key(key(
            KeyCode::Char('w'),
            KeyEventKind::Press,
            KeyModifiers::NONE,
        ));
        assert!(!app.watch);
        assert!(app.watch_error.is_none());
        assert!(app.pending_source.is_none());
    }

    #[test]
    fn commits_latest_snapshot_when_content_keeps_changing() {
        let mut app = app("initial", true);
        let started = Instant::now();

        assert!(!app.stage_source("first".to_owned(), started));
        assert!(!app.stage_source("second".to_owned(), started + WATCH_POLL_INTERVAL));
        assert!(app.stage_source("latest".to_owned(), started + WATCH_MAX_SETTLE_TIME));
        assert_eq!(app.source, "latest");
        assert!(app.pending_source.is_none());
    }
}
