//! alternate screen 上の表示状態、キー操作、ファイル再読み込みを管理する。
//! watch 中に読み込みが失敗した場合は最後に描画できた内容を保持し、同じパスを
//! 次のポーリング周期で読み直す。同じ内容を連続して読めた時点で表示へ反映し、
//! 内容が変わり続ける場合は1秒後に最新候補を反映する。`r` はこの周期と安定待ちを
//! 使わず、その場で読んで反映する。
//!
//! diff の基準 (snapshot) と表示層の状態もここが所有する。行の対応付けと行内の
//! 変更 range は `diff` モジュールが計算し、この層は色と描画だけを持つ。

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
    style::{Color, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor},
    terminal::{
        self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
        enable_raw_mode,
    },
};

use crate::{
    diff::{self, DiffLayers, DiffRow, RowKind},
    renderer, style,
    style::{
        StyledLine, TextStyle, display_width, line_with_bg, line_with_bg_ranges,
        line_with_search_highlight, slice_line,
    },
};

const WATCH_POLL_INTERVAL: Duration = Duration::from_millis(250);
const WATCH_MAX_SETTLE_TIME: Duration = Duration::from_secs(1);

/// ステータス行の左右ゾーンが接触しない最小の間隔。
const STATUS_GAP: usize = 2;

/// `?` で開くキー一覧。押すキーではなく用途の近さで並べる。
const HELP_KEYS: &[(&str, &str)] = &[
    ("j k ↓ ↑ e y", "Scroll one line"),
    ("f b PgDn PgUp", "Scroll one page"),
    ("g G", "Jump to top / bottom"),
    ("h l ← →", "Scroll 4 columns"),
    ("H L Shift+← →", "Scroll half a screen width"),
    ("0", "Back to the first column"),
    ("/", "Search"),
    ("n N", "Next / previous match"),
    ("r", "Reload the file now"),
    ("w", "Toggle watch mode"),
    ("s", "Take or discard a snapshot"),
    ("d", "Cycle diff view"),
    ("Esc", "Leave diff view"),
    ("q", "Quit"),
];

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
    /// `?` で開くキー一覧。本文の上へパネルを重ねる間だけ入る。
    Help,
}

/// diff 表示の状態。New は現在の内容、Old は snapshot の内容を、どちらも
/// 変更箇所ハイライトつきの整列済み行で描画する。`d` が New⇄Old を反転し、
/// 共通行が同じ画面位置に来るため連打すると変更箇所だけが入れ替わって見える。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DiffView {
    Off,
    New,
    Old,
}

/// diff 層の背景色。`line_bg` は変更行全体へうっすら、`emphasis_bg` は行内の
/// 変更 range だけ濃く敷く。`filler_bg` は相手層にだけ行がある位置の空行で、
/// 相手層側の色相を暗くしたもの (新層では削除位置 = 赤系、旧層では追加位置 =
/// 緑系) にして、そこで行が増減したことを示す。
struct DiffPalette {
    line_bg: Color,
    emphasis_bg: Color,
    filler_bg: Color,
}

impl DiffPalette {
    const NEW: Self = Self {
        line_bg: Color::Rgb {
            r: 18,
            g: 52,
            b: 26,
        },
        emphasis_bg: Color::Rgb {
            r: 34,
            g: 98,
            b: 48,
        },
        filler_bg: Color::Rgb {
            r: 44,
            g: 22,
            b: 22,
        },
    };
    const OLD: Self = Self {
        line_bg: Color::Rgb {
            r: 62,
            g: 26,
            b: 26,
        },
        emphasis_bg: Color::Rgb {
            r: 116,
            g: 44,
            b: 44,
        },
        filler_bg: Color::Rgb {
            r: 22,
            g: 42,
            b: 26,
        },
    };
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
    /// 直近の読み込み失敗 (watch の周期読み込みと `r` の双方)。読み込み成功か
    /// watch の切り替えで None へ戻る。
    read_error: Option<String>,
    /// 安定待ち中の内容と開始時刻。確定、読み込み失敗、watch 無効化で破棄する。
    pending_source: Option<PendingSource>,
    /// diff の基準となる Markdown 原文。`s` で取得と破棄が反転する。watch の
    /// 有効化は基準が無いときだけ現在の内容を自動で入れ、手動の基準を上書きしない。
    snapshot: Option<String>,
    /// 表示中の層。Off 以外になるのは snapshot がある間だけで、破棄時は Off へ戻す。
    diff_view: DiffView,
    /// snapshot と現在の整列結果。source・snapshot・幅のいずれかが変わると None
    /// へ戻り、次に diff 表示が必要になった時点で作り直す。
    diff: Option<DiffLayers>,
    /// 次のキー入力まで表示する操作ヒント。snapshot 無しで `d` を押した場合に出す。
    hint: Option<String>,
}

impl App {
    fn new(path: PathBuf, source: String, width: u16, height: u16, watch: bool) -> Self {
        let lines = renderer::render_markdown(&source, width as usize);
        let snapshot = watch.then(|| source.clone());
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
            read_error: None,
            pending_source: None,
            snapshot,
            diff_view: DiffView::Off,
            diff: None,
            hint: None,
        }
    }

    fn resize(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
        self.refresh_render();
    }

    fn handle_key(&mut self, key: KeyEvent) -> bool {
        // ヒントは1キー分だけ見せる。どのキーでも次の入力で消える。
        self.hint = None;

        match &mut self.mode {
            // 一覧を開いたまま操作を続けられると、どのキーが効いたのか
            // 画面から読めなくなる。最初の 1 打で必ず閉じ、その打鍵は
            // 本文へ渡さない。
            Mode::Help => self.mode = Mode::Normal,
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
                    self.read_error = None;
                    self.pending_source = None;
                    if self.watch && self.snapshot.is_none() {
                        // watch 開始時点を diff の基準にする。
                        self.snapshot = Some(self.source.clone());
                    }
                }
                KeyCode::Char('s')
                    if key.kind == KeyEventKind::Press && key.modifiers == KeyModifiers::NONE =>
                {
                    self.toggle_snapshot();
                }
                KeyCode::Char('d') if key.modifiers == KeyModifiers::NONE => {
                    // Repeat も受ける: d を押しっぱなしにすると新旧が交互に
                    // 切り替わり続け、ブリンク比較がそのまま成立する。
                    self.advance_diff_view();
                }
                KeyCode::Char('r')
                    if key.kind == KeyEventKind::Press && key.modifiers == KeyModifiers::NONE =>
                {
                    self.reload_now();
                }
                // `?` は多くの配列で Shift+/ なので、修飾キーは問わない。
                KeyCode::Char('?') => self.mode = Mode::Help,
                KeyCode::Esc if self.diff_view != DiffView::Off => {
                    self.set_diff_view(DiffView::Off);
                }
                _ => {}
            },
        }

        false
    }

    /// `s` の反転動作。基準が無ければ今表示している内容 (ディスクではない) を
    /// 基準として取り、あれば破棄して diff 表示も強制的に閉じる。
    fn toggle_snapshot(&mut self) {
        if self.snapshot.is_some() {
            self.snapshot = None;
            self.diff = None;
            self.set_diff_view(DiffView::Off);
        } else {
            self.snapshot = Some(self.source.clone());
            self.diff = None;
        }
    }

    /// `d` の遷移: Off → New、以降は New⇄Old。基準が無い間は表示を変えず
    /// ステータス行へヒントだけ出す。
    fn advance_diff_view(&mut self) {
        if self.snapshot.is_none() {
            self.hint = Some("no snapshot (press s)".to_owned());
            return;
        }
        let next = match self.diff_view {
            DiffView::Off => DiffView::New,
            DiffView::New => DiffView::Old,
            DiffView::Old => DiffView::New,
        };
        self.set_diff_view(next);
    }

    /// 表示層を切り替え、検索対象と表示範囲を新しい行リストへ合わせ直す。
    /// query は保持され、マッチ行だけが層のテキストで再計算される。
    fn set_diff_view(&mut self, view: DiffView) {
        self.diff_view = view;
        if view != DiffView::Off {
            self.ensure_diff();
        }
        self.rebuild_matches();
        self.clamp_top();
        self.clamp_left();
    }

    /// snapshot と現在の描画行から整列結果を作る。キャッシュがあれば何もしない。
    fn ensure_diff(&mut self) {
        if self.diff.is_some() {
            return;
        }
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let old_lines = renderer::render_markdown(snapshot, self.width as usize);
        self.diff = Some(diff::compute(&old_lines, &self.lines));
    }

    /// 描画・検索・スクロールが対象にする層。diff 表示が Off の間は None を
    /// 返し、呼び出し側は通常の描画行 (`lines`) を使う。
    fn active_layer(&self) -> Option<(&[DiffRow], &'static DiffPalette)> {
        let diff = self.diff.as_ref()?;
        match self.diff_view {
            DiffView::Off => None,
            DiffView::New => Some((&diff.new_rows, &DiffPalette::NEW)),
            DiffView::Old => Some((&diff.old_rows, &DiffPalette::OLD)),
        }
    }

    fn line_count(&self) -> usize {
        self.active_layer()
            .map_or(self.lines.len(), |(rows, _)| rows.len())
    }

    fn draw(&mut self, stdout: &mut io::Stdout) -> Result<()> {
        self.clamp_top();
        self.clamp_left();
        let body_height = self.body_height();
        let width = self.width as usize;

        queue!(stdout, Clear(ClearType::All))?;
        for row in 0..body_height {
            let line_index = self.top + row as usize;
            queue!(stdout, MoveTo(0, row))?;

            if let Some((rows, palette)) = self.active_layer() {
                let Some(diff_row) = rows.get(line_index) else {
                    continue;
                };
                let (composed, row_bg) = compose_diff_row(diff_row, palette, &self.query);
                let visible = slice_line(&composed, self.left, width);
                draw_line(stdout, &visible)?;
                if let Some(bg) = row_bg {
                    // 変更行と filler は行末まで背景を敷き、行の増減が起きた
                    // 位置をブリンク時に面で追えるようにする。
                    let pad = width.saturating_sub(display_width(&visible.plain_text()));
                    if pad > 0 {
                        let filler = StyledLine::styled(
                            " ".repeat(pad),
                            TextStyle {
                                bg: Some(bg),
                                ..TextStyle::normal()
                            },
                        );
                        draw_line(stdout, &filler)?;
                    }
                }
            } else if let Some(line) = self.lines.get(line_index) {
                let highlighted = line_with_search_highlight(line, &self.query);
                let visible = slice_line(&highlighted, self.left, width);
                draw_line(stdout, &visible)?;
            }
        }

        if matches!(self.mode, Mode::Help) {
            self.draw_help(stdout)?;
        }

        self.draw_status(stdout)?;
        stdout.flush()?;
        Ok(())
    }

    fn draw_status(&self, stdout: &mut io::Stdout) -> Result<()> {
        let row = self.height.saturating_sub(1);
        let bar = line_with_bg(&self.status_line(self.width as usize), self.status_bg());

        queue!(stdout, MoveTo(0, row))?;
        draw_line(stdout, &bar)
    }

    /// バーの地色。検索入力中と読み込み失敗中だけ色相を変える。失敗表示は
    /// 次に読めるまで残り続けるので、手元の入力を隠さないよう検索を優先する。
    fn status_bg(&self) -> Color {
        if matches!(self.mode, Mode::SearchInput(_)) {
            style::STATUS_BG_SEARCH
        } else if self.read_error.is_some() {
            style::STATUS_BG_ERROR
        } else {
            style::STATUS_BG
        }
    }

    /// ステータス行を左右 2 ゾーンで組み、間を空白で埋めて `width` ちょうどに
    /// する。左は「何を見ているか」(ファイル名と状態バッジ)、右は「どこに
    /// いるか」(行番号と割合)。
    ///
    /// 狭い端末では右ゾーンを `?:help` → 行番号 の順に落として割合だけを残し、
    /// 左ゾーンは幅の 2/3 で頭打ちにする。長いファイル名や長いエラー文で位置
    /// 表示が押し出されることはなく、収まらなかった左ゾーンは `…` 付きで
    /// 切れるので、途中までのバッジをそのままの値と読み違えない。
    fn status_line(&self, width: usize) -> StyledLine {
        let mut line = truncate_line(&self.status_left(), (width * 2 / 3).max(1));
        let left_width = display_width(&line.plain_text());
        let right = self
            .status_right_forms()
            .into_iter()
            .find(|form| left_width + STATUS_GAP + display_width(&form.plain_text()) <= width)
            .unwrap_or_else(StyledLine::empty);

        let gap = width
            .saturating_sub(left_width)
            .saturating_sub(display_width(&right.plain_text()));
        line.push(" ".repeat(gap), TextStyle::normal());
        for span in right.spans {
            line.push(span.text, span.style);
        }
        line
    }

    /// 左ゾーン。ファイル名を主役に置き、以降は既定と違う状態だけをバッジで
    /// 足す。watch 無効や snapshot 無しは既定なので出さない。
    fn status_left(&self) -> StyledLine {
        let mut line = StyledLine::empty();
        line.push(" ", TextStyle::normal());

        if let Mode::SearchInput(input) = &self.mode {
            // 端末カーソルは隠したままなので、入力位置は自前の桁で示す。
            line.push(format!("/{input}"), TextStyle::chrome_strong(style::TEXT));
            line.push("▏", TextStyle::chrome(style::TEXT));
            return line;
        }

        let filename = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("markdown");
        line.push(filename, TextStyle::chrome_strong(style::TEXT));

        if self.watch {
            push_badge(&mut line, "● watch", style::GREEN);
        }
        match self.diff_view {
            // diff 表示中は snapshot があることが前提なので、snap は畳む。
            // 色は diff 層の色相 (新 = 緑、旧 = 赤) に合わせる。
            DiffView::New => push_badge(&mut line, "▌diff new", style::GREEN),
            DiffView::Old => push_badge(&mut line, "▌diff old", style::ROSE),
            DiffView::Off if self.snapshot.is_some() => {
                push_badge(&mut line, "◆ snap", style::MAUVE)
            }
            DiffView::Off => {}
        }
        if self.left > 0 {
            push_badge(&mut line, &format!("col {}", self.left + 1), style::SUBTEXT);
        }
        if !self.query.is_empty() {
            let (text, color) = self.search_badge();
            push_badge(&mut line, &text, color);
        }
        if let Some(error) = &self.read_error {
            push_badge(&mut line, &format!("▲ {error}"), style::ROSE);
        }
        if let Some(hint) = &self.hint {
            push_badge(&mut line, hint, style::GOLD);
        }

        line
    }

    /// 検索バッジ。ヒット無しだけ色を変えて、空振りが分かるようにする。
    /// `n`/`N` で現在位置が決まる前は総数だけを出す。
    fn search_badge(&self) -> (String, Color) {
        match (self.matches.len(), self.match_index) {
            (0, _) => (format!("/{} no match", self.query), style::ROSE),
            (total, Some(index)) => (
                format!("/{} {}/{total}", self.query, index + 1),
                style::SUBTEXT,
            ),
            (total, None) => (format!("/{} {total}", self.query), style::SUBTEXT),
        }
    }

    /// 右ゾーンの候補を広い順に返す。`status_line` が入る最初のものを選ぶ。
    fn status_right_forms(&self) -> [StyledLine; 3] {
        let total = self.line_count();
        let current = (self.top + self.body_height() as usize).min(total);
        let percent = (current * 100).checked_div(total).unwrap_or(100);
        // 総行数の桁で右詰めし、スクロール中に右ゾーンの左端を揺らさない。
        let position = format!("{current:>width$}/{total}", width = total.to_string().len());
        let percent = format!("{percent:>3}%");

        let form = |help: bool, position_shown: bool| {
            let mut line = StyledLine::empty();
            if help {
                line.push("?:help", TextStyle::chrome(style::GREY));
                line.push("  ", TextStyle::normal());
            }
            if position_shown {
                line.push(&position, TextStyle::chrome(style::SUBTEXT));
                line.push("  ", TextStyle::normal());
            }
            line.push(&percent, TextStyle::chrome(style::SUBTEXT));
            line.push(" ", TextStyle::normal());
            line
        };

        [form(true, true), form(false, true), form(false, false)]
    }

    /// キー一覧を本文の上へ中央寄せで重ねる。枠が収まらない端末では何も
    /// 描かず、ステータス行の `?:help` だけを残す。
    fn draw_help(&self, stdout: &mut io::Stdout) -> Result<()> {
        let widest = |widths: &mut dyn Iterator<Item = usize>| widths.max().unwrap_or(0);
        let key_width = widest(&mut HELP_KEYS.iter().map(|(keys, _)| display_width(keys)));
        let action_width = widest(&mut HELP_KEYS.iter().map(|(_, action)| display_width(action)));
        // 左右の余白 2 + 2 と、キー列・説明列の間の 2。
        let inner_width = key_width + action_width + 6;

        let (Some(free_x), Some(free_y)) = (
            (self.width as usize).checked_sub(inner_width + 2),
            (self.body_height() as usize).checked_sub(HELP_KEYS.len() + 2),
        ) else {
            return Ok(());
        };
        let (x, y) = ((free_x / 2) as u16, (free_y / 2) as u16);

        let border = TextStyle::chrome(style::GREY);
        let title = " keys ";
        let mut top = StyledLine::empty();
        top.push("┌─", border);
        top.push(title, TextStyle::chrome_strong(style::TEXT));
        top.push(
            "─".repeat(inner_width.saturating_sub(1 + display_width(title))),
            border,
        );
        top.push("┐", border);

        let mut bottom = StyledLine::empty();
        bottom.push("└", border);
        bottom.push("─".repeat(inner_width), border);
        bottom.push("┘", border);

        let rows = std::iter::once(top)
            .chain(HELP_KEYS.iter().map(|(keys, action)| {
                let mut row = StyledLine::empty();
                row.push("│", border);
                row.push("  ", TextStyle::normal());
                row.push(
                    format!("{keys:<key_width$}"),
                    TextStyle::chrome(style::GOLD),
                );
                row.push("  ", TextStyle::normal());
                row.push(
                    format!("{action:<action_width$}"),
                    TextStyle::chrome(style::SUBTEXT),
                );
                row.push("  ", TextStyle::normal());
                row.push("│", border);
                row
            }))
            .chain(std::iter::once(bottom));

        for (offset, row) in rows.enumerate() {
            queue!(stdout, MoveTo(x, y + offset as u16))?;
            draw_line(stdout, &line_with_bg(&row, style::STATUS_BG))?;
        }
        Ok(())
    }

    /// watch が有効ならファイルを読み直し、画面に見える状態が変わったかを返す。
    ///
    /// 新しい内容は `stage_source` の安定条件を満たしてから renderer へ渡す。
    /// 読み込み失敗は `read_error` のみを更新し、最後に確定した表示を保持する。
    fn reload_if_changed(&mut self) -> bool {
        if !self.watch {
            return false;
        }

        match fs::read_to_string(&self.path) {
            Ok(source) => {
                let recovered = self.read_error.take().is_some();
                self.stage_source(source, Instant::now()) || recovered
            }
            Err(error) => {
                self.pending_source = None;
                let error = error.to_string();
                if self.read_error.as_deref() == Some(&error) {
                    return false;
                }
                self.read_error = Some(error);
                true
            }
        }
    }

    /// `r` の即時再読み込み。watch の周期と安定待ちを使わず、読めた内容を
    /// その場で反映して画面に見える状態が変わったかを返す。基準は動かさない。
    fn reload_now(&mut self) -> bool {
        self.pending_source = None;
        match fs::read_to_string(&self.path) {
            Ok(source) => {
                let recovered = self.read_error.take().is_some();
                self.replace_source(source) || recovered
            }
            Err(error) => {
                let error = error.to_string();
                if self.read_error.as_deref() == Some(&error) {
                    return false;
                }
                self.read_error = Some(error);
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
    /// 検索語とスクロール位置は引き継ぐ。diff 表示中は基準を保ったまま新層側
    /// だけが更新され、整列とハイライトが作り直される。
    fn replace_source(&mut self, source: String) -> bool {
        if source == self.source {
            return false;
        }

        self.source = source;
        self.refresh_render();
        true
    }

    /// source または表示幅の変化後に、描画行・整列結果・検索対象を作り直し、
    /// 新しい行数・表示幅から外れたスクロール位置だけを末尾へ戻す。
    fn refresh_render(&mut self) {
        self.lines = renderer::render_markdown(&self.source, self.width as usize);
        self.diff = None;
        if self.diff_view != DiffView::Off {
            self.ensure_diff();
        }
        self.rebuild_matches();
        self.clamp_top();
        self.clamp_left();
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
        self.line_count()
            .saturating_sub(self.body_height() as usize)
    }

    fn clamp_top(&mut self) {
        self.top = self.top.min(self.max_top());
    }

    fn max_left(&self) -> usize {
        let width = self.width as usize;
        let overflow = |text: String| display_width(&text).saturating_sub(width);
        match self.active_layer() {
            Some((rows, _)) => rows
                .iter()
                .map(|row| overflow(row.line.plain_text()))
                .max()
                .unwrap_or(0),
            None => self
                .lines
                .iter()
                .map(|line| overflow(line.plain_text()))
                .max()
                .unwrap_or(0),
        }
    }

    fn clamp_left(&mut self) {
        self.left = self.left.min(self.max_left());
    }

    /// 表示中の行リスト (diff 表示中はその層、通常は描画行) から検索マッチを
    /// 作り直す。filler 行は空文字列なので自然にマッチしない。
    fn rebuild_matches(&mut self) {
        let query = self.query.to_lowercase();
        if query.is_empty() {
            self.matches.clear();
            self.match_index = None;
            return;
        }

        let matched =
            |(index, text): (usize, String)| text.to_lowercase().contains(&query).then_some(index);
        let matches = match self.active_layer() {
            Some((rows, _)) => rows
                .iter()
                .map(|row| row.line.plain_text())
                .enumerate()
                .filter_map(matched)
                .collect(),
            None => self
                .lines
                .iter()
                .map(StyledLine::plain_text)
                .enumerate()
                .filter_map(matched)
                .collect(),
        };
        self.matches = matches;
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

/// diff 行へスタイルを重ねる。重ね順は 行 tint → 行内強調 → 検索で、検索が
/// 最上位になる。返り値の Color は行末まで敷く背景で、Common では None。
fn compose_diff_row(
    row: &DiffRow,
    palette: &DiffPalette,
    query: &str,
) -> (StyledLine, Option<Color>) {
    match &row.kind {
        RowKind::Common => (line_with_search_highlight(&row.line, query), None),
        RowKind::Changed { emphasis } => {
            let tinted = line_with_bg(&row.line, palette.line_bg);
            let emphasized = line_with_bg_ranges(&tinted, emphasis, palette.emphasis_bg);
            (
                line_with_search_highlight(&emphasized, query),
                Some(palette.line_bg),
            )
        }
        RowKind::Filler => (StyledLine::empty(), Some(palette.filler_bg)),
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

/// 状態バッジを 1 個ぶん左ゾーンへ足す。太字はファイル名だけに残し、
/// バッジは色で区別させる。
fn push_badge(line: &mut StyledLine, text: &str, color: Color) {
    line.push("  ", TextStyle::normal());
    line.push(text, TextStyle::chrome(color));
}

/// 表示幅 `max` へ収まるよう末尾を落とし、落とした場合だけ `…` を付ける。
fn truncate_line(line: &StyledLine, max: usize) -> StyledLine {
    if display_width(&line.plain_text()) <= max {
        return line.clone();
    }
    let mut output = slice_line(line, 0, max.saturating_sub(1));
    output.push("…", TextStyle::chrome(style::SUBTEXT));
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

    fn press(code: KeyCode) -> KeyEvent {
        key(code, KeyEventKind::Press, KeyModifiers::NONE)
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

        app.handle_key(press(KeyCode::Char('w')));

        assert!(!app.watch);
        assert!(matches!(&app.mode, Mode::SearchInput(input) if input == "w"));
    }

    #[test]
    fn keeps_snapshot_keys_as_search_input() {
        let mut app = app("before", false);
        app.mode = Mode::SearchInput(String::new());

        app.handle_key(press(KeyCode::Char('s')));
        app.handle_key(press(KeyCode::Char('d')));
        app.handle_key(press(KeyCode::Char('r')));

        assert!(app.snapshot.is_none());
        assert_eq!(app.diff_view, DiffView::Off);
        assert!(matches!(&app.mode, Mode::SearchInput(input) if input == "sdr"));
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
        assert!(app.read_error.is_some());
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
        assert!(app.read_error.is_none());

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
        assert!(!app.status_line(80).plain_text().contains("● watch"));

        app.watch = true;
        assert!(app.status_line(80).plain_text().contains("● watch"));
        app.read_error = Some("read failed".to_owned());
        app.pending_source = Some(PendingSource {
            source: "pending".to_owned(),
            first_seen: Instant::now(),
        });
        assert!(app.status_line(80).plain_text().contains("▲ read failed"));
        assert_eq!(app.status_bg(), style::STATUS_BG_ERROR);

        app.handle_key(press(KeyCode::Char('w')));
        assert!(!app.watch);
        assert!(app.read_error.is_none());
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

    #[test]
    fn snapshot_toggle_and_diff_flip_flow() {
        let mut app = app("alpha\n\nbeta", false);
        assert!(app.snapshot.is_none());

        app.handle_key(press(KeyCode::Char('d')));
        assert_eq!(app.diff_view, DiffView::Off);
        assert_eq!(app.hint.as_deref(), Some("no snapshot (press s)"));

        app.handle_key(press(KeyCode::Char('s')));
        assert_eq!(app.snapshot.as_deref(), Some("alpha\n\nbeta"));
        assert!(app.hint.is_none());
        assert_eq!(app.diff_view, DiffView::Off);

        app.handle_key(press(KeyCode::Char('d')));
        assert_eq!(app.diff_view, DiffView::New);
        assert!(app.diff.is_some());
        app.handle_key(press(KeyCode::Char('d')));
        assert_eq!(app.diff_view, DiffView::Old);
        app.handle_key(press(KeyCode::Char('d')));
        assert_eq!(app.diff_view, DiffView::New);

        app.handle_key(press(KeyCode::Esc));
        assert_eq!(app.diff_view, DiffView::Off);
        assert!(app.snapshot.is_some());
    }

    #[test]
    fn discarding_snapshot_forces_diff_off() {
        let mut app = app("content", false);

        app.handle_key(press(KeyCode::Char('s')));
        app.handle_key(press(KeyCode::Char('d')));
        assert_eq!(app.diff_view, DiffView::New);

        app.handle_key(press(KeyCode::Char('s')));
        assert!(app.snapshot.is_none());
        assert_eq!(app.diff_view, DiffView::Off);
        assert!(app.diff.is_none());
    }

    #[test]
    fn watch_enable_takes_snapshot_only_when_missing() {
        let mut app = app("first", false);

        app.handle_key(press(KeyCode::Char('w')));
        assert!(app.watch);
        assert_eq!(app.snapshot.as_deref(), Some("first"));

        app.replace_source("second".to_owned());
        app.handle_key(press(KeyCode::Char('w')));
        assert!(!app.watch);
        assert_eq!(app.snapshot.as_deref(), Some("first"));

        app.handle_key(press(KeyCode::Char('w')));
        assert!(app.watch);
        assert_eq!(app.snapshot.as_deref(), Some("first"));
    }

    #[test]
    fn watch_startup_takes_initial_snapshot() {
        let watching = app("initial", true);
        assert_eq!(watching.snapshot.as_deref(), Some("initial"));

        let plain = app("initial", false);
        assert!(plain.snapshot.is_none());
    }

    #[test]
    fn manual_reload_applies_immediately_and_reports_errors() {
        let file = TestFile::new("after");
        let mut app = App::new(file.path.clone(), "before".to_owned(), 80, 24, false);
        app.handle_key(press(KeyCode::Char('s')));

        app.handle_key(press(KeyCode::Char('r')));
        assert_eq!(app.source, "after");
        assert_eq!(app.snapshot.as_deref(), Some("before"));

        fs::remove_file(&file.path).unwrap();
        app.handle_key(press(KeyCode::Char('r')));
        assert!(app.read_error.is_some());
        assert_eq!(app.source, "after");

        fs::write(&file.path, "recovered").unwrap();
        app.handle_key(press(KeyCode::Char('r')));
        assert!(app.read_error.is_none());
        assert_eq!(app.source, "recovered");
        assert_eq!(app.snapshot.as_deref(), Some("before"));
    }

    #[test]
    fn manual_reload_bypasses_stability_wait() {
        let file = TestFile::new("initial");
        let mut app = App::new(file.path.clone(), "initial".to_owned(), 80, 24, true);
        app.pending_source = Some(PendingSource {
            source: "half written".to_owned(),
            first_seen: Instant::now(),
        });

        fs::write(&file.path, "settled").unwrap();
        assert!(app.reload_now());

        assert_eq!(app.source, "settled");
        assert!(app.pending_source.is_none());
    }

    #[test]
    fn search_targets_the_displayed_layer() {
        let mut app = app("grape", false);
        app.handle_key(press(KeyCode::Char('s')));
        app.replace_source("apple".to_owned());
        app.query = "grape".to_owned();

        app.handle_key(press(KeyCode::Char('d')));
        assert_eq!(app.diff_view, DiffView::New);
        assert!(app.matches.is_empty());

        app.handle_key(press(KeyCode::Char('d')));
        assert_eq!(app.diff_view, DiffView::Old);
        assert_eq!(app.matches.len(), 1);
    }

    #[test]
    fn status_shows_snapshot_and_diff_state() {
        let mut app = app("content", false);
        assert!(!app.status_line(80).plain_text().contains("snap"));

        app.handle_key(press(KeyCode::Char('s')));
        assert!(app.status_line(80).plain_text().contains("◆ snap"));

        // diff 表示中は snapshot があることが前提なので、snap バッジは畳む。
        app.handle_key(press(KeyCode::Char('d')));
        let status = app.status_line(80).plain_text();
        assert!(status.contains("▌diff new"));
        assert!(!status.contains("◆ snap"));

        app.handle_key(press(KeyCode::Char('d')));
        assert!(app.status_line(80).plain_text().contains("▌diff old"));
    }

    #[test]
    fn status_fills_the_width_and_sheds_the_right_zone_when_narrow() {
        let mut app = app("content", false);
        app.query = "content".to_owned();
        app.rebuild_matches();

        let wide = app.status_line(80).plain_text();
        assert!(wide.contains("?:help"));
        assert!(wide.contains("/content 1"));
        assert_eq!(display_width(&wide), 80);

        // 幅が減るごとに ?:help、行番号の順で落ち、割合は最後まで残る。
        let narrow = app.status_line(30).plain_text();
        assert!(!narrow.contains("?:help"));
        assert!(narrow.trim_end().ends_with('%'));
        assert_eq!(display_width(&narrow), 30);

        let tiny = app.status_line(20).plain_text();
        assert!(tiny.trim_end().ends_with("100%"));
        assert_eq!(display_width(&tiny), 20);
    }

    #[test]
    fn search_status_reports_a_miss() {
        let mut app = app("content", false);
        app.query = "nothing here".to_owned();
        app.rebuild_matches();

        assert!(
            app.status_line(80)
                .plain_text()
                .contains("/nothing here no match")
        );
    }

    #[test]
    fn help_opens_on_question_mark_and_closes_on_the_next_key() {
        let mut app = app("content", false);

        app.handle_key(key(
            KeyCode::Char('?'),
            KeyEventKind::Press,
            KeyModifiers::SHIFT,
        ));
        assert!(matches!(app.mode, Mode::Help));

        // 閉じるための打鍵は本文へ渡さない。
        app.handle_key(press(KeyCode::Char('G')));
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(app.top, 0);
    }

    #[test]
    fn diff_layers_align_and_viewport_follows_active_layer() {
        let mut app = app("a\n\nb\n\nc", false);
        app.handle_key(press(KeyCode::Char('s')));
        app.replace_source("a\n\nc".to_owned());

        app.handle_key(press(KeyCode::Char('d')));
        let (new_rows, _) = app.active_layer().unwrap();
        let aligned = new_rows.len();
        assert_eq!(app.line_count(), aligned);

        app.handle_key(press(KeyCode::Char('d')));
        let (old_rows, _) = app.active_layer().unwrap();
        assert_eq!(old_rows.len(), aligned);

        app.handle_key(press(KeyCode::Esc));
        assert_eq!(app.line_count(), app.lines.len());
    }
}
