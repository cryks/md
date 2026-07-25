//! alternate screen 上の表示状態、キー操作、ファイル再読み込みを管理する。
//! watch 中に読み込みが失敗した場合は最後に描画できた内容を保持し、同じパスを
//! 次のポーリング周期で読み直す。同じ内容を連続して読めた時点で表示へ反映し、
//! 内容が変わり続ける場合は1秒後に最新候補を反映する。`r` はこの周期と安定待ちを
//! 使わず、その場で読んで反映する。
//!
//! diff の基準 (snapshot) と表示層の状態もここが所有する。行の対応付けと行内の
//! 変更 range は `diff` モジュールが計算し、この層は色と描画だけを持つ。

use std::{
    cmp::Ordering,
    fs,
    io::{self, Write},
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::Result;
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    execute, queue,
    style::{Color, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor},
    terminal::{
        self, BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate,
        EnterAlternateScreen, LeaveAlternateScreen, ScrollDown, ScrollUp, disable_raw_mode,
        enable_raw_mode,
    },
};

use crate::{
    diff::{self, DiffLayers, DiffRow, RowKind},
    renderer,
    renderer::Heading,
    style,
    style::{
        StyledLine, TextStyle, display_width, line_with_bg, line_with_bg_ranges,
        line_with_search_highlight, slice_line,
    },
};

const WATCH_POLL_INTERVAL: Duration = Duration::from_millis(250);
const WATCH_MAX_SETTLE_TIME: Duration = Duration::from_secs(1);

/// ホイール 1 ノッチのスクロール量。
const WHEEL_SCROLL_LINES: isize = 3;

/// セクション一覧に一度に出す項目数の上限。sticky の下に残る高さがこれより
/// 狭い端末では、そちらに合わせて縮める。
const SECTION_MENU_HEIGHT: usize = 10;

/// ステータス行の左右ゾーンが接触しない最小の間隔。
const STATUS_GAP: usize = 2;

/// `?` で開くキー一覧。押すキーではなく用途の近さで並べる。
const HELP_KEYS: &[(&str, &str)] = &[
    ("j k ↓ ↑ e y", "Scroll one line"),
    ("f b PgDn PgUp", "Scroll one page"),
    ("g G", "Jump to top / bottom"),
    ("Tab Shift+Tab", "Open the section menu"),
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
                Event::Mouse(mouse) => {
                    should_draw = app.handle_mouse(mouse);
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
        // マウスを掴むと端末側のドラッグ選択とホイール送りが止まる。選択は
        // Shift (端末によっては Option) 併用で従来どおり使え、ホイールは
        // `App::handle_mouse` が本文スクロールとして受け直す。
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture, Hide)?;
        Ok(Self { stdout })
    }

    fn writer(&mut self) -> &mut io::Stdout {
        &mut self.stdout
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = execute!(
            self.stdout,
            Show,
            DisableMouseCapture,
            LeaveAlternateScreen,
            ResetColor
        );
        let _ = disable_raw_mode();
    }
}

#[derive(Debug)]
enum Mode {
    Normal,
    SearchInput(String),
    /// `?` で開くキー一覧。本文の上へパネルを重ねる間だけ入る。
    Help,
    /// sticky から開くセクション一覧。
    Section(SectionMenu),
}

/// セクション一覧の状態。`anchor` は開いた時点の見出しチェーン
/// (`App::active_headings` の index、浅い順) で、`depth` はそのうち何段目を
/// 対象にしているかを指す。sticky は `anchor[..depth]` と選択中の見出しで
/// 描くので、選択や階層を変えても領域の高さは動かない。
#[derive(Debug)]
struct SectionMenu {
    /// 一覧を開いた時点の `top`。`Esc` はここへ戻す。
    origin_top: usize,
    anchor: Vec<usize>,
    depth: usize,
    /// 対象と同じ親を持つ同レベルの見出し。対象自身を必ず含み、h1 のように
    /// 親が無いレベルでは文書内の全 h1 になる。
    items: Vec<usize>,
    /// 選択中の `items` index。開いた直後を除き、本文はこの見出しの位置へ
    /// 寄せてある。
    selected: usize,
    /// 表示窓の先頭 `items` index。ホイールで動かし、選択が窓の外へ出たときは
    /// `keep_selection_in_window` が最小限だけ追いつかせる。窓に入りきらない
    /// 項目数のときだけ 0 以外になり、`items.len()` から窓の行数を引いた値を
    /// 超えない。
    offset: usize,
    /// `origin_top` が属していたセクションの `items` index。文書の先頭など
    /// どのセクションにも入っていない位置や、別の枝を選んだ後は None。
    current: Option<usize>,
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

/// diff 表示が使う整列結果と、両層の見出し位置。`old_headings` /
/// `new_headings` の `line` は `DiffLayers` の写像で整列後の行へ移してあり、
/// 通常表示の `App::headings` と同じ意味 (画面に出る行の index) で扱える。
struct DiffState {
    layers: DiffLayers,
    old_headings: Vec<Heading>,
    new_headings: Vec<Heading>,
}

struct PendingSource {
    /// 確定前に読めた最新の内容。連続更新中はポーリングごとに置き換える。
    source: String,
    /// 最初の未確定変更を読んだ時刻。内容が変わり続けても更新せず、確定期限に使う。
    first_seen: Instant,
}

/// 直前に端末へ送ったフレームを決めた入力。次のフレームとの差が `top` だけ
/// なら、画面の既存行をスクロール領域の移動で再利用できる (`scroll_frame`)。
/// `render_gen` 以外のフィールドはそれぞれ本文の描画内容を直接変える入力で、
/// どれか 1 つでも違えばフル再描画に落とす。
struct FrameState {
    /// 描画行の世代。`refresh_render` (内容・幅の変化) ごとに進む。
    render_gen: u64,
    top: usize,
    left: usize,
    width: u16,
    height: u16,
    diff_view: DiffView,
    query: String,
    sticky: Vec<usize>,
    /// sticky 領域が確保していた見出しスロット数。見出しの数が同じでも、
    /// diff の層の切り替えで空きスロットの数だけが変わることがある。
    sticky_slots: usize,
}

struct App {
    path: PathBuf,
    source: String,
    lines: Vec<StyledLine>,
    /// `lines` 内の h1-h3 の位置。sticky 表示の計算だけが読み、`lines` と
    /// 同時に作り直す。
    headings: Vec<Heading>,
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
    diff: Option<DiffState>,
    /// 次のキー入力まで表示する操作ヒント。snapshot 無しで `d` を押した場合に出す。
    hint: Option<String>,
    /// 描画行の世代。`refresh_render` が進め、`FrameState` との比較だけに使う。
    render_gen: u64,
    /// 直前フレームの入力。初回描画の前は None。
    last_frame: Option<FrameState>,
}

impl App {
    fn new(path: PathBuf, source: String, width: u16, height: u16, watch: bool) -> Self {
        let doc = renderer::render_markdown(&source, width as usize);
        let snapshot = watch.then(|| source.clone());
        Self {
            path,
            source,
            lines: doc.lines,
            headings: doc.headings,
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
            render_gen: 0,
            last_frame: None,
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
            // キー一覧を開いたまま操作を続けられると、どのキーが効いたのか
            // 画面から読めなくなる。最初の 1 打で必ず閉じ、その打鍵は
            // 本文へ渡さない。
            Mode::Help => self.mode = Mode::Normal,
            Mode::Section(_) => self.handle_section_key(key),
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
                KeyCode::Tab => self.open_section_menu_from_sticky(false),
                KeyCode::BackTab => self.open_section_menu_from_sticky(true),
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

    /// セクション一覧が開いている間のキー操作。割り当てのないキーは捨てる:
    /// 一覧は能動的に開いた状態なので、打ち間違いで閉じて位置を失わせない。
    fn handle_section_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.cancel_section_menu(),
            KeyCode::Enter => self.mode = Mode::Normal,
            KeyCode::Down | KeyCode::Char('j') => self.step_section_selection(1),
            KeyCode::Up | KeyCode::Char('k') => self.step_section_selection(-1),
            KeyCode::Tab => self.step_section_level(1),
            KeyCode::BackTab => self.step_section_level(-1),
            _ => {}
        }
    }

    /// sticky のチェーンからセクション一覧を開く。`shallowest` が true なら
    /// 一番浅い段、false なら一番深い段を対象にする。sticky が空になるのは
    /// 最初の見出しより上にいるときで、そこでは文書の最初の見出しを対象に
    /// して兄弟を並べる。
    fn open_section_menu_from_sticky(&mut self, shallowest: bool) {
        if self.active_headings().is_empty() {
            return;
        }

        let chain = sticky_chain(self.active_headings(), self.top, self.max_sticky_count());
        let target = if shallowest {
            chain.first().copied()
        } else {
            chain.last().copied()
        };
        self.open_section_menu(target.unwrap_or(0));
    }

    /// `target` (`active_headings` の index) のセクション一覧を開く。すでに
    /// 開いている場合は対象だけ差し替え、`origin_top` は最初に開いたときの
    /// ものを保つ。
    ///
    /// 本文はここでは動かさない。開いた時点の選択は今いるセクションなので、
    /// 寄せてしまうと Tab を押しただけで画面が飛ぶ。
    fn open_section_menu(&mut self, target: usize) {
        let origin_top = match &self.mode {
            Mode::Section(menu) => menu.origin_top,
            _ => self.top,
        };
        let mut anchor = ancestors(self.active_headings(), target);
        anchor.push(target);
        // sticky に載らない浅い段は落とす。通常の sticky と同じ規則で深い方を
        // 残す。対象自身は必ず要るので、載らない高さでも 1 段は保つ。
        anchor.drain(..anchor.len().saturating_sub(self.max_sticky_count().max(1)));
        let depth = anchor.len() - 1;

        self.mode = Mode::Section(SectionMenu {
            origin_top,
            anchor,
            depth,
            items: Vec::new(),
            selected: 0,
            offset: 0,
            current: None,
        });
        self.rebuild_section_items(depth, target, origin_top);
    }

    /// `anchor` の `depth` 段目にある `target` を対象として一覧を作り直す。
    fn rebuild_section_items(&mut self, depth: usize, target: usize, origin_top: usize) {
        let headings = self.active_headings();
        let items = siblings(headings, target);
        let selected = items.iter().position(|index| *index == target).unwrap_or(0);
        // 今いるセクションの印は開いた位置から引く。対象を別の枝へ移した
        // 後はその枝が一覧に入らないので None になる。
        let chain = sticky_chain(headings, origin_top, usize::MAX);
        let current = items.iter().position(|index| chain.contains(index));

        if let Mode::Section(menu) = &mut self.mode {
            menu.depth = depth;
            menu.items = items;
            menu.selected = selected;
            menu.offset = 0;
            menu.current = current;
        }
        self.keep_selection_in_window();
    }

    /// 選択が表示窓から出ていたら、最小限だけ窓を動かして戻す。
    fn keep_selection_in_window(&mut self) {
        let Mode::Section(menu) = &self.mode else {
            return;
        };
        let rows = self.section_menu_height(menu);
        if rows == 0 {
            return;
        }

        let Mode::Section(menu) = &mut self.mode else {
            return;
        };
        if menu.selected < menu.offset {
            menu.offset = menu.selected;
        } else if menu.selected >= menu.offset + rows {
            menu.offset = menu.selected + 1 - rows;
        }
    }

    /// 一覧の表示窓だけを送り、動いたかを返す。選択は動かさないので本文も
    /// 動かない。
    fn scroll_section_menu(&mut self, delta: isize) -> bool {
        let Mode::Section(menu) = &self.mode else {
            return false;
        };
        let last = menu
            .items
            .len()
            .saturating_sub(self.section_menu_height(menu));

        let Mode::Section(menu) = &mut self.mode else {
            return false;
        };
        let next = menu.offset.saturating_add_signed(delta).min(last);
        if next == menu.offset {
            return false;
        }
        menu.offset = next;
        true
    }

    /// 対象の段を深い方 (`delta` > 0) か浅い方へ動かす。段は循環する。
    fn step_section_level(&mut self, delta: isize) {
        let Mode::Section(menu) = &self.mode else {
            return;
        };
        let levels = menu.anchor.len();
        if levels <= 1 {
            return;
        }
        let depth = (menu.depth + levels).saturating_add_signed(delta) % levels;
        let (target, origin_top) = (menu.anchor[depth], menu.origin_top);

        self.rebuild_section_items(depth, target, origin_top);
        self.follow_section_selection();
    }

    /// 選択を 1 つ送る。端では巻き戻す: 止めると末尾から先頭へ戻るのに
    /// 一覧の長さぶんの打鍵が要る。
    fn step_section_selection(&mut self, delta: isize) {
        let Mode::Section(menu) = &mut self.mode else {
            return;
        };
        let len = menu.items.len();
        menu.selected = (menu.selected + len).saturating_add_signed(delta) % len;
        self.keep_selection_in_window();
        self.follow_section_selection();
    }

    /// 一覧を閉じ、開く前の位置へ戻す。
    fn cancel_section_menu(&mut self) {
        let Mode::Section(menu) = &self.mode else {
            return;
        };
        let origin_top = menu.origin_top;
        self.mode = Mode::Normal;
        self.top = origin_top;
    }

    /// 一覧の `index` を選び、その場で確定する。
    fn select_section_item(&mut self, index: usize) {
        if let Mode::Section(menu) = &mut self.mode {
            menu.selected = index;
        }
        self.follow_section_selection();
        self.mode = Mode::Normal;
    }

    /// 選択中の見出しの直後の行が本文領域の先頭に来るよう `top` を寄せる。
    fn follow_section_selection(&mut self) {
        let Mode::Section(menu) = &self.mode else {
            return;
        };
        self.top = self.section_placement(menu).0;
        self.clamp_top();
    }

    /// 選択中の見出しを sticky の最下段へ載せるための `top` とスロット数。
    ///
    /// 見出しより上に敷ける行が足りない (文書の先頭に近い) ときは段を減らし、
    /// 1 段も敷けなければ sticky を出さずに見出し行そのものを本文の先頭に
    ///置く。`top` と段数を一箇所で決めることで、一覧を開いている間は選択を
    /// 動かしても本文の先頭が常に見出しの直後になる。
    fn section_placement(&self, menu: &SectionMenu) -> (usize, usize) {
        let line = self.active_headings()[menu.items[menu.selected]].line;
        (1..=menu.depth + 1)
            .rev()
            .find_map(|slots| Some(((line + 1).checked_sub(sticky_area_height(slots))?, slots)))
            .unwrap_or((line, 0))
    }

    /// マウス入力を処理し、画面を描き直す必要があるかを返す。
    ///
    /// マウスを掴んでいる間はポインタを動かすだけでもイベントが届く。表示が
    /// 変わらない入力で true を返すと、その都度フレームを組み直すことになる
    /// ので、実際に動いた場合だけ true にする。
    fn handle_mouse(&mut self, event: MouseEvent) -> bool {
        let scroll = match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                return self.handle_click(event.row as usize);
            }
            MouseEventKind::ScrollDown => WHEEL_SCROLL_LINES,
            MouseEventKind::ScrollUp => -WHEEL_SCROLL_LINES,
            _ => return false,
        };

        match self.mode {
            Mode::Normal => self.wheel_scroll(scroll),
            Mode::Section(_) => self.scroll_section_menu(scroll),
            _ => false,
        }
    }

    /// 左クリックを処理し、画面を描き直す必要があるかを返す。
    ///
    /// 見出しを押すと一覧が開く / 対象が差し替わる。一覧の項目を押すとその
    /// 場で確定し、それ以外の場所を押すと開く前の位置へ戻して閉じる。
    fn handle_click(&mut self, row: usize) -> bool {
        match &self.mode {
            Mode::Normal => match self.heading_at_row(row) {
                Some(target) => {
                    self.open_section_menu(target);
                    true
                }
                None => false,
            },
            Mode::Section(menu) => {
                let area_top = self.section_menu_top(menu);
                let height = self.section_menu_height(menu);
                let item = row
                    .checked_sub(area_top)
                    .filter(|offset| *offset < height)
                    .map(|offset| menu.offset + offset);
                // 一覧を開いた見出しと、いまその位置に出ている選択中の見出し。
                // 選択を動かすと sticky の最下段が差し替わるので、両方を
                // 「開いた場所」として扱う。
                let opened_on = [menu.anchor[menu.depth], menu.items[menu.selected]];

                match (item, self.heading_at_row(row)) {
                    (Some(index), _) => self.select_section_item(index),
                    // 開いた場所をもう一度押したときはトグルとして閉じる。
                    (None, Some(target)) if opened_on.contains(&target) => {
                        self.cancel_section_menu()
                    }
                    (None, Some(target)) => self.open_section_menu(target),
                    (None, None) => self.cancel_section_menu(),
                }
                true
            }
            _ => false,
        }
    }

    /// 縦へ `delta` 行スクロールし、位置が動いたかを返す。
    fn wheel_scroll(&mut self, delta: isize) -> bool {
        let before = self.top;
        self.scroll_lines(delta);
        self.top != before
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
    ///
    /// 見出しは両層とも整列後の行 index へ写して持つ。snapshot 側の見出しは
    /// ここでしか手に入らない (基準の描画行は保持しない) ので、層と同時に作る。
    fn ensure_diff(&mut self) {
        if self.diff.is_some() {
            return;
        }
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let old_doc = renderer::render_markdown(snapshot, self.width as usize);
        let layers = diff::compute(&old_doc.lines, &self.lines);
        self.diff = Some(DiffState {
            old_headings: project_headings(&old_doc.headings, &layers.old_map),
            new_headings: project_headings(&self.headings, &layers.new_map),
            layers,
        });
    }

    /// 描画・検索・スクロールが対象にする層。diff 表示が Off の間は None を
    /// 返し、呼び出し側は通常の描画行 (`lines`) を使う。
    fn active_layer(&self) -> Option<(&[DiffRow], &'static DiffPalette)> {
        let diff = self.diff.as_ref()?;
        match self.diff_view {
            DiffView::Off => None,
            DiffView::New => Some((&diff.layers.new_rows, &DiffPalette::NEW)),
            DiffView::Old => Some((&diff.layers.old_rows, &DiffPalette::OLD)),
        }
    }

    /// sticky とセクション移動が対象にする見出し。行 index は常に表示中の
    /// 行リスト (diff 表示中はその層) に対応する。
    fn active_headings(&self) -> &[Heading] {
        match (&self.diff, self.diff_view) {
            (Some(diff), DiffView::New) => &diff.new_headings,
            (Some(diff), DiffView::Old) => &diff.old_headings,
            _ => &self.headings,
        }
    }

    fn line_count(&self) -> usize {
        self.active_layer()
            .map_or(self.lines.len(), |(rows, _)| rows.len())
    }

    fn draw(&mut self, stdout: &mut io::Stdout) -> Result<()> {
        let frame = self.render_frame()?;
        stdout.write_all(&frame)?;
        stdout.flush()?;
        Ok(())
    }

    /// 1 フレームをバッファへ組み立てて返す。呼び出し側が 1 回の write で
    /// 流すことが前提で、stdout へ直接 queue すると内部バッファを跨いだ
    /// 時点で途中までの絵が端末に届き、塗り替え中の行が一瞬見える。
    ///
    /// フレームは 2 通りの組み立て方を持つ。直前フレームとの差が縦
    /// スクロールだけなら `scroll_frame` が画面の既存行を端末側の移動で
    /// 再利用し、露出した行とステータス行だけを描く。それ以外はフル
    /// 再描画で、全画面クリアはせず全行を上書きして行末の残りだけ消す。
    /// 各行はフレーム内で一度だけ塗り、sticky に覆われる行の本文は最初
    /// から描かない。フレームの反映が途中で切れても画面に出るのは前
    /// フレームの行であって、空白行や上書き前の中間状態ではない。
    /// Synchronized Update 対応端末ではフレーム全体が原子的に反映される。
    fn render_frame(&mut self) -> Result<Vec<u8>> {
        self.clamp_top();
        self.clamp_left();
        let (sticky, slots) = self.sticky_view();

        let mut frame: Vec<u8> = Vec::new();
        queue!(frame, BeginSynchronizedUpdate)?;

        if !self.scroll_frame(&mut frame, &sticky, slots)? {
            self.draw_sticky(&mut frame, &sticky, slots)?;
            for row in sticky_area_height(slots)..self.body_height() as usize {
                self.draw_body_row(&mut frame, row)?;
            }
            match &self.mode {
                Mode::Help => self.draw_help(&mut frame)?,
                Mode::Section(menu) => self.draw_section_menu(&mut frame, menu)?,
                _ => {}
            }
        }

        self.draw_status(&mut frame)?;
        queue!(frame, EndSynchronizedUpdate)?;

        self.last_frame = Some(FrameState {
            render_gen: self.render_gen,
            top: self.top,
            left: self.left,
            width: self.width,
            height: self.height,
            diff_view: self.diff_view,
            query: self.query.clone(),
            sticky,
            sticky_slots: slots,
        });
        Ok(frame)
    }

    /// 直前フレームとの差が縦スクロールだけの場合、本文領域をスクロール
    /// 領域 (DECSTBM) として端末側で `delta` 行ずらし、露出した行だけを
    /// 描いて true を返す。sticky・覆われていない既存行・ヘルプ以外の
    /// 画面は動かないので、送るバイト数が全行描画に比べて桁で減る。
    ///
    /// 適用条件: Normal モードで、本文の描画内容を決める入力 (内容世代・
    /// left・幅・高さ・diff 層・検索語・sticky チェーン) が前フレームと
    /// 一致し、移動量が本文領域の高さ未満であること。ヘルプ表示中は
    /// スクロールキー自体が届かない (最初の 1 打で閉じる) ので、パネルを
    /// 消し忘れる経路はない。
    fn scroll_frame(&self, frame: &mut Vec<u8>, sticky: &[usize], slots: usize) -> Result<bool> {
        let Some(last) = &self.last_frame else {
            return Ok(false);
        };
        if !matches!(self.mode, Mode::Normal)
            || last.render_gen != self.render_gen
            || last.left != self.left
            || last.width != self.width
            || last.height != self.height
            || last.diff_view != self.diff_view
            || last.query != self.query
            || last.sticky != sticky
            || last.sticky_slots != slots
        {
            return Ok(false);
        }

        let area_top = sticky_area_height(slots);
        let body_height = self.body_height() as usize;
        let area_height = body_height - area_top;
        let delta = self.top as isize - last.top as isize;
        if delta == 0 || delta.unsigned_abs() >= area_height {
            return Ok(false);
        }

        // DECSTBM は crossterm に無いので raw で送る (1 始まりの閉区間)。
        // SU/SD は指定領域内だけを動かし、ステータス行と sticky は領域外に
        // なるので触られない。解除 (CSI r) でカーソルは home へ動くが、
        // 以降の描画は毎回 MoveTo するので影響しない。
        write!(frame, "\x1b[{};{}r", area_top + 1, body_height)?;
        let amount = delta.unsigned_abs() as u16;
        let exposed = if delta > 0 {
            queue!(frame, ScrollUp(amount))?;
            body_height - amount as usize..body_height
        } else {
            queue!(frame, ScrollDown(amount))?;
            area_top..area_top + amount as usize
        };
        write!(frame, "\x1b[r")?;

        for row in exposed {
            self.draw_body_row(frame, row)?;
        }
        Ok(true)
    }

    /// 本文 1 行を画面の `row` 行目へ塗る。表示する行が無い位置は行クリア
    /// だけになり、前フレームの内容が残らない。
    fn draw_body_row(&self, frame: &mut Vec<u8>, row: usize) -> Result<()> {
        let line_index = self.top + row;
        let width = self.width as usize;
        queue!(frame, MoveTo(0, row as u16))?;

        if let Some((rows, palette)) = self.active_layer() {
            if let Some(diff_row) = rows.get(line_index) {
                let (composed, row_bg) = compose_diff_row(diff_row, palette, &self.query);
                let visible = slice_line(&composed, self.left, width);
                draw_line(frame, &visible)?;
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
                        draw_line(frame, &filler)?;
                    }
                }
            }
        } else if let Some(line) = self.lines.get(line_index) {
            let highlighted = line_with_search_highlight(line, &self.query);
            let visible = slice_line(&highlighted, self.left, width);
            draw_line(frame, &visible)?;
        }

        queue!(frame, Clear(ClearType::UntilNewLine))?;
        Ok(())
    }

    /// sticky 表示する見出しの行 index を h1 側から返す。diff 表示中は表示層
    /// の見出しになるので、行 index はそのまま画面の行として使える。
    fn sticky_heading_lines(&self) -> Vec<usize> {
        let headings = self.active_headings();
        sticky_chain(headings, self.top, self.max_sticky_count())
            .into_iter()
            .map(|index| headings[index].line)
            .collect()
    }

    /// いま画面に出ている sticky の見出し行 (行 index) とスロット数。
    ///
    /// セクション一覧を開いている間は、対象より深い段を落として最下段を選択中
    /// の見出しに差し替えたものになる。段数は選択に依らず `section_placement`
    /// が決めるので、選択や階層を変えても一覧の位置と本文の開始行が動かない。
    fn sticky_view(&self) -> (Vec<usize>, usize) {
        let Mode::Section(menu) = &self.mode else {
            return (self.sticky_heading_lines(), self.sticky_slots());
        };

        let (_, slots) = self.section_placement(menu);
        if slots == 0 {
            return (Vec::new(), 0);
        }

        let headings = self.active_headings();
        let lines = menu.anchor[menu.depth + 1 - slots..menu.depth]
            .iter()
            .chain(std::iter::once(&menu.items[menu.selected]))
            .map(|index| headings[*index].line)
            .collect();
        (lines, slots)
    }

    /// 画面 `row` にある見出し (`active_headings` の index)。
    ///
    /// sticky では見出し行とその直上の padding を同じ当たりにする。見出しは
    /// 1 行しかなく、クリックの的として細すぎるため。最下段の padding は本文
    /// との境界なのでどの見出しにも寄せない。本文側は見出し行そのものだけを
    /// 見る。前後の空行はセクションの区切りであってどちらにも属さない。
    fn heading_at_row(&self, row: usize) -> Option<usize> {
        let (sticky, slots) = self.sticky_view();
        let line = if row < sticky_area_height(slots) {
            *sticky.get(row / 2)?
        } else {
            self.top + row
        };
        self.active_headings()
            .iter()
            .position(|heading| heading.line == line)
    }

    /// sticky に出せる見出しの上限。本文が半分以上隠れる端末では、いま読んで
    /// いる場所に近い深いレベルを優先し、浅い方から削る。
    fn max_sticky_count(&self) -> usize {
        let budget = self.body_height() as usize / 2;
        budget.saturating_sub(1) / 2
    }

    /// sticky 領域が確保する見出しスロット数。diff 表示中は両層の多い方に
    /// そろえる: 見出しを増減した文書では層ごとにチェーンの長さが変わり、
    /// 揃えないと本文の開始行がずれてブリンク比較で無関係な行まで動く。
    fn sticky_slots(&self) -> usize {
        let max_count = self.max_sticky_count();
        let count = |headings: &[Heading]| sticky_chain(headings, self.top, max_count).len();
        match (&self.diff, self.diff_view) {
            (Some(diff), DiffView::New | DiffView::Old) => {
                count(&diff.old_headings).max(count(&diff.new_headings))
            }
            _ => count(self.active_headings()),
        }
    }

    fn sticky_height(&self) -> usize {
        sticky_area_height(self.sticky_slots())
    }

    /// 見出しチェーンを本文の上へ重ねる。行を覆う overlay なのでスクロール
    /// 位置の解釈は変えず、覆われた行は上へ抜ける前に必ず一度は非覆域を
    /// 通る (ページ送りだけ `scroll_pages` が送り量で補正する)。
    ///
    /// 領域は `slots` 個の見出しぶんを確保し、`indexes` が足りない分は空行に
    /// する。diff の両層で見出しの数が違っても本文の開始行が動かない。
    fn draw_sticky(&self, frame: &mut Vec<u8>, indexes: &[usize], slots: usize) -> Result<()> {
        if slots == 0 {
            return Ok(());
        }

        let width = self.width as usize;
        let blank = StyledLine::styled(
            " ".repeat(width),
            TextStyle {
                bg: Some(style::STATUS_BG),
                ..TextStyle::normal()
            },
        );

        // 上端 padding 1 行 + (見出し + padding 1 行) の繰り返し。見出し間の
        // padding は上下で共有され、最後の 1 行が本文との境界になる。
        let mut row = 0u16;
        queue!(frame, MoveTo(0, row))?;
        draw_line(frame, &blank)?;
        row += 1;

        for slot in 0..slots {
            queue!(frame, MoveTo(0, row))?;
            match indexes.get(slot) {
                Some(&line_index) => draw_line(frame, &self.sticky_line(line_index))?,
                None => draw_line(frame, &blank)?,
            }
            row += 1;
            queue!(frame, MoveTo(0, row))?;
            draw_line(frame, &blank)?;
            row += 1;
        }
        Ok(())
    }

    /// sticky に出す見出し 1 行。本文と同じ体裁のまま地色だけ差し替え、
    /// 行末まで背景を敷いて帯として閉じる。
    fn sticky_line(&self, line_index: usize) -> StyledLine {
        let width = self.width as usize;
        let source = match self.active_layer() {
            Some((rows, _)) => &rows[line_index].line,
            None => &self.lines[line_index],
        };
        let highlighted = line_with_search_highlight(source, &self.query);
        let visible = slice_line(&highlighted, self.left, width);
        let mut composed = line_with_bg(&visible, style::STATUS_BG);
        let pad = width.saturating_sub(display_width(&composed.plain_text()));
        composed.push(
            " ".repeat(pad),
            TextStyle {
                bg: Some(style::STATUS_BG),
                ..TextStyle::normal()
            },
        );
        composed
    }

    /// セクション一覧の 1 行目が来る画面 row。sticky の直下に続けて描く。
    fn section_menu_top(&self, menu: &SectionMenu) -> usize {
        sticky_area_height(self.section_placement(menu).1)
    }

    /// セクション一覧が使う高さ。
    fn section_menu_height(&self, menu: &SectionMenu) -> usize {
        let area_top = self.section_menu_top(menu);
        let available = (self.body_height() as usize).saturating_sub(area_top);
        SECTION_MENU_HEIGHT.min(menu.items.len()).min(available)
    }

    /// セクション一覧を sticky の直下へ重ねる。sticky と同じ地色の帯として
    /// 続け、見出しは `#` と罫線を落としたテキストだけを出す。選択行は反転し、
    /// 開いた時点でいたセクションには印を付ける。
    fn draw_section_menu(&self, frame: &mut Vec<u8>, menu: &SectionMenu) -> Result<()> {
        let headings = self.active_headings();
        let width = self.width as usize;
        let area_top = self.section_menu_top(menu);
        let height = self.section_menu_height(menu);

        for row in 0..height {
            let index = menu.offset + row;
            let text = TextStyle {
                fg: Some(style::TEXT),
                bg: Some(style::STATUS_BG),
                reverse: index == menu.selected,
                ..TextStyle::normal()
            };
            let marker = TextStyle {
                fg: Some(style::GOLD),
                ..text
            };

            let mut line = StyledLine::empty();
            line.push(" ", text);
            line.push(
                if menu.current == Some(index) {
                    "•"
                } else {
                    " "
                },
                marker,
            );
            line.push(" ", text);
            line.push(&headings[menu.items[index]].text, text);

            let mut visible = slice_line(&line, 0, width);
            let pad = width.saturating_sub(display_width(&visible.plain_text()));
            visible.push(" ".repeat(pad), text);

            queue!(frame, MoveTo(0, (area_top + row) as u16))?;
            draw_line(frame, &visible)?;
        }
        Ok(())
    }

    fn draw_status(&self, frame: &mut Vec<u8>) -> Result<()> {
        let row = self.height.saturating_sub(1);
        let bar = line_with_bg(&self.status_line(self.width as usize), self.status_bg());

        queue!(frame, MoveTo(0, row))?;
        draw_line(frame, &bar)
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

        if matches!(self.mode, Mode::Section(_)) {
            // `q` も `?` も効かない間なので、抜け方と階層の変え方を出す。
            for (key, action) in [("Enter", "go"), ("Esc", "cancel"), ("Tab", "level")] {
                line.push(key, TextStyle::chrome(style::GOLD));
                line.push(format!(": {action}"), TextStyle::chrome(style::SUBTEXT));
                line.push("  ", TextStyle::normal());
            }
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
    fn draw_help(&self, frame: &mut Vec<u8>) -> Result<()> {
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
            queue!(frame, MoveTo(x, y + offset as u16))?;
            draw_line(frame, &line_with_bg(&row, style::STATUS_BG))?;
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
        let doc = renderer::render_markdown(&self.source, self.width as usize);
        self.lines = doc.lines;
        self.headings = doc.headings;
        // 一覧が持つ見出し index は作り直した行を指さないので閉じる。
        if matches!(self.mode, Mode::Section(_)) {
            self.mode = Mode::Normal;
        }
        self.render_gen += 1;
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
        // sticky に覆われた行はページ単位のジャンプだと一度も画面へ出ない
        // まま上へ抜けるので、覆い分だけ送りを縮めて読み飛ばしを防ぐ。
        let amount = (self.body_height() as usize)
            .saturating_sub(self.sticky_height())
            .max(1) as isize;
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

/// 見出し `count` 個の sticky 領域の高さ。見出しの上下に padding を 1 行
/// ずつ置き、隣接する見出し間では共有するので `count > 0` で `2 * count + 1`。
/// 0 なら領域ごと出さない。
fn sticky_area_height(count: usize) -> usize {
    if count == 0 { 0 } else { 2 * count + 1 }
}

/// 画面上端が `top` にあるときの見出しチェーンを `headings` の index で返す。
///
/// チェーンは「境界より上にある見出し」をスタックで畳んだもの: 深いレベルは、
/// 後から同じか浅いレベルの見出しが来た時点でそのセクションが閉じているので
/// 落とす。`max_count` を超える分は浅い方から削る。
fn sticky_chain(headings: &[Heading], top: usize, max_count: usize) -> Vec<usize> {
    let chain_above = |boundary: usize| {
        let mut chain: Vec<usize> = Vec::new();
        for (index, heading) in headings.iter().enumerate() {
            if heading.line >= boundary {
                break;
            }
            while chain
                .last()
                .is_some_and(|last| headings[*last].level >= heading.level)
            {
                chain.pop();
            }
            chain.push(index);
        }
        chain.drain(..chain.len().saturating_sub(max_count));
        chain
    };

    // 画面上端を境界にした基本チェーン。この領域に覆われる範囲には
    // 次セクションの見出しが入りうる。本文が見え始めているのに前の
    // セクションを出し続けないよう、境界を覆われる範囲の下端まで
    // 一度だけ広げて計算し直す。広げた結果が自身の領域からはみ出す
    // (= まだ見えている見出し行と二重表示になる) 場合は基本チェーンへ
    // 戻す。境界を動的に追い続けると領域の伸縮でチェーンが振動して
    // 定まらないことがあるため、拡張は一度で打ち切る。
    //
    // 広げ幅は段が 1 つ増えた後の高さで測る。見出しが 1 つ昇格すれば領域も
    // その分伸びるので、伸びる前の高さで測ると、ちょうど境界にいる見出しを
    // 取りこぼす。上限に達している場合は増えないので伸ばさない。上に見出しが
    // 1 つも無い位置では領域そのものが無いので、拡張の起点も作らない。
    let baseline = chain_above(top);
    let reach = match baseline.len() {
        0 => 0,
        shown => (shown + 1).min(max_count),
    };
    let candidate = chain_above(top + sticky_area_height(reach));
    let extent = top + sticky_area_height(candidate.len());
    if candidate.iter().all(|index| headings[*index].line < extent) {
        candidate
    } else {
        baseline
    }
}

/// `index` の見出しと同じ親を持つ同レベルの見出しを、文書順に返す。自分自身を
/// 必ず含む。より浅い見出しに出会うまでが同じ親の範囲で、h1 のように親が無い
/// レベルでは文書内の全 h1 になる。
fn siblings(headings: &[Heading], index: usize) -> Vec<usize> {
    let level = headings[index].level;
    let mut before = Vec::new();
    for candidate in (0..index).rev() {
        match headings[candidate].level.cmp(&level) {
            Ordering::Less => break,
            Ordering::Equal => before.push(candidate),
            Ordering::Greater => {}
        }
    }

    before.reverse();
    let mut items = before;
    items.push(index);
    for (candidate, heading) in headings.iter().enumerate().skip(index + 1) {
        match heading.level.cmp(&level) {
            Ordering::Less => break,
            Ordering::Equal => items.push(candidate),
            Ordering::Greater => {}
        }
    }
    items
}

/// `index` の見出しの祖先を浅い順に返す。手前へさかのぼりながら、それまでに
/// 見たどれよりも浅い見出しだけを拾う。
fn ancestors(headings: &[Heading], index: usize) -> Vec<usize> {
    let mut chain = Vec::new();
    let mut level = headings[index].level;
    for candidate in (0..index).rev() {
        if headings[candidate].level < level {
            level = headings[candidate].level;
            chain.push(candidate);
        }
    }
    chain.reverse();
    chain
}

/// 見出しの行 index を `map` (入力行 → 整列後の行) で写した複製を返す。
fn project_headings(headings: &[Heading], map: &[usize]) -> Vec<Heading> {
    headings
        .iter()
        .map(|heading| Heading {
            line: map[heading.line],
            ..heading.clone()
        })
        .collect()
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

fn draw_line(frame: &mut Vec<u8>, line: &StyledLine) -> Result<()> {
    for span in &line.spans {
        apply_style(frame, span.style)?;
        queue!(frame, Print(&span.text))?;
    }
    queue!(
        frame,
        SetAttribute(crossterm::style::Attribute::Reset),
        ResetColor
    )?;
    Ok(())
}

fn apply_style(frame: &mut Vec<u8>, style: TextStyle) -> Result<()> {
    queue!(
        frame,
        SetAttribute(crossterm::style::Attribute::Reset),
        ResetColor
    )?;
    if let Some(fg) = style.fg {
        queue!(frame, SetForegroundColor(fg))?;
    }
    if let Some(bg) = style.bg {
        queue!(frame, SetBackgroundColor(bg))?;
    }
    for attribute in style.attributes() {
        queue!(frame, SetAttribute(attribute))?;
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

    /// h1 → h2 → h3 → h2 の章立て。見出し位置はテストが `headings` から
    /// 引くので、本文の行数はスクロール余地が十分にあればよい。
    fn sectioned_app() -> App {
        let mut source = String::from("# A\n");
        source.push_str(&"a\n".repeat(20));
        source.push_str("## B\n");
        source.push_str(&"b\n".repeat(20));
        source.push_str("### C\n");
        source.push_str(&"c\n".repeat(20));
        source.push_str("## D\n");
        source.push_str(&"d\n".repeat(20));
        app(&source, false)
    }

    #[test]
    fn sticky_shows_ancestor_headings_above_the_viewport() {
        let mut app = sectioned_app();
        let headings = app.headings.clone();
        assert!(app.sticky_heading_lines().is_empty());

        app.top = headings[2].line + 2;
        assert_eq!(
            app.sticky_heading_lines(),
            vec![headings[0].line, headings[1].line, headings[2].line]
        );
    }

    #[test]
    fn sticky_drops_subsections_closed_by_a_later_heading() {
        let mut app = sectioned_app();
        let headings = app.headings.clone();

        // ## D を過ぎると ### C のセクションは閉じている。
        app.top = headings[3].line + 1;
        assert_eq!(
            app.sticky_heading_lines(),
            vec![headings[0].line, headings[3].line]
        );
    }

    #[test]
    fn sticky_promotes_a_heading_covered_by_the_area() {
        let mut app = sectioned_app();
        let headings = app.headings.clone();

        // ## D はまだ top より下だが sticky 領域に覆われる位置にいる。
        // 本文は D のセクションへ入っているので、B ではなく D を出す。
        app.top = headings[3].line - 2;
        assert_eq!(
            app.sticky_heading_lines(),
            vec![headings[0].line, headings[3].line]
        );
    }

    fn bytes_contain(frame: &[u8], needle: &[u8]) -> bool {
        frame.windows(needle.len()).any(|window| window == needle)
    }

    #[test]
    fn line_scroll_moves_the_screen_instead_of_repainting() {
        let mut app = sectioned_app();
        app.top = app.headings[2].line + 2;

        // 初回はフル再描画で、sticky の h1 罫線 (━) を含む。
        let full = app.render_frame().unwrap();
        assert!(bytes_contain(&full, "━".as_bytes()));

        // 差が縦 1 行のフレームはスクロールコマンド (SU) で画面を再利用し、
        // sticky を塗り直さない。
        app.scroll_lines(1);
        let scrolled = app.render_frame().unwrap();
        assert!(bytes_contain(&scrolled, b"\x1b[1S"));
        assert!(!bytes_contain(&scrolled, "━".as_bytes()));

        app.scroll_lines(-1);
        let back = app.render_frame().unwrap();
        assert!(bytes_contain(&back, b"\x1b[1T"));
    }

    #[test]
    fn content_change_forces_a_full_repaint() {
        let mut app = sectioned_app();
        app.top = app.headings[2].line + 2;
        let _ = app.render_frame().unwrap();

        let source = format!("{}extra\n", app.source);
        assert!(app.replace_source(source));
        app.scroll_lines(1);

        let frame = app.render_frame().unwrap();
        assert!(!bytes_contain(&frame, b"\x1b[1S"));
        assert!(bytes_contain(&frame, "━".as_bytes()));
    }

    #[test]
    fn sticky_transition_forces_a_full_repaint() {
        let mut app = sectioned_app();
        let d = app.headings[3].line;
        app.top = d - 6;
        let _ = app.render_frame().unwrap();

        // このスクロールで sticky が [A, B, C] から [A, D] に変わる。
        app.scroll_lines(4);
        let frame = app.render_frame().unwrap();
        assert!(!bytes_contain(&frame, b"\x1b[4S"));
        assert!(bytes_contain(&frame, "━".as_bytes()));
    }

    #[test]
    fn sticky_never_vanishes_while_scrolling_through_sections() {
        // 見出しが 1 つでも上へ消えていれば、そこは必ずどこかのセクションの
        // 中なので、どのスクロール位置でもヘッダは表示され続ける。
        let mut app = sectioned_app();
        let first_heading = app.headings[0].line;

        for top in 0..=app.max_top() {
            app.top = top;
            assert_eq!(
                app.sticky_heading_lines().is_empty(),
                top <= first_heading,
                "top={top}"
            );
        }
    }

    /// sticky に出ている見出しのテキスト。表示中の層から引くので、diff の
    /// 層を切り替えると内容も切り替わる。
    fn sticky_texts(app: &App) -> Vec<String> {
        app.sticky_heading_lines()
            .into_iter()
            .map(|line| match app.active_layer() {
                Some((rows, _)) => rows[line].line.plain_text(),
                None => app.lines[line].plain_text(),
            })
            .collect()
    }

    #[test]
    fn sticky_shows_the_headings_of_the_layer_on_screen() {
        let mut app = sectioned_app();
        app.handle_key(press(KeyCode::Char('s')));
        app.replace_source(app.source.replace("### C\n", "### C2\n"));

        app.handle_key(press(KeyCode::Char('d')));
        assert_eq!(app.diff_view, DiffView::New);
        app.top = app.active_headings()[2].line + 2;
        assert!(sticky_texts(&app)[2].starts_with("### C2"));

        // 層は整列済みなので同じ top が旧層の同じ位置を指す。
        app.handle_key(press(KeyCode::Char('d')));
        assert_eq!(app.diff_view, DiffView::Old);
        assert!(sticky_texts(&app)[2].starts_with("### C "));
    }

    #[test]
    fn sticky_slots_match_the_taller_diff_layer() {
        let mut app = sectioned_app();
        app.handle_key(press(KeyCode::Char('s')));
        // 新層では ### C が本文になり、チェーンが 1 段浅くなる。
        app.replace_source(app.source.replace("### C\n", "c\n"));

        app.handle_key(press(KeyCode::Char('d')));
        app.top = app.diff.as_ref().unwrap().old_headings[2].line + 2;
        let shown = app.sticky_heading_lines().len();
        let height = app.sticky_height();

        app.handle_key(press(KeyCode::Char('d')));
        assert_eq!(app.sticky_heading_lines().len(), shown + 1);
        assert_eq!(app.sticky_height(), height);
    }

    #[test]
    fn page_scroll_shrinks_by_the_sticky_overlay_height() {
        let mut app = sectioned_app();
        let start = app.headings[2].line + 2;
        app.top = start;
        assert_eq!(app.sticky_height(), 7);

        app.handle_key(press(KeyCode::PageDown));
        assert_eq!(app.top, start + app.body_height() as usize - 7);
    }

    #[test]
    fn short_terminal_keeps_only_the_deepest_sticky_headings() {
        let mut app = sectioned_app();
        let headings = app.headings.clone();
        app.top = headings[1].line + 2;

        // body 9 行 → 許容 4 行 → 見出し 1 個 (3 行) だけ。浅い h1 が落ちる。
        app.height = 10;
        assert_eq!(app.sticky_heading_lines(), vec![headings[1].line]);

        // 見出し 1 個ぶんも許容できない高さでは sticky ごと出さない。
        app.height = 5;
        assert!(app.sticky_heading_lines().is_empty());
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

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn wheel_scrolls_the_body_only_when_the_view_can_move() {
        let mut app = sectioned_app();

        assert!(app.handle_mouse(mouse(MouseEventKind::ScrollDown, 0, 0)));
        assert!(app.top > 0);

        // 末尾では位置が動かないので描き直しも要求しない。ポインタの移動
        // イベントも同じで、これを true にすると全画面の塗り直しが続く。
        app.top = app.max_top();
        assert!(!app.handle_mouse(mouse(MouseEventKind::ScrollDown, 0, 0)));
        assert!(!app.handle_mouse(mouse(MouseEventKind::Moved, 4, 4)));
    }

    #[test]
    fn wheel_is_ignored_while_a_panel_is_open() {
        let mut app = sectioned_app();
        app.handle_key(press(KeyCode::Char('?')));

        assert!(!app.handle_mouse(mouse(MouseEventKind::ScrollDown, 0, 0)));
        assert_eq!(app.top, 0);
    }

    /// 章をまたいだ兄弟関係を見るための章立て:
    /// `# A / ## B / ### C / ### D / ## E / # F / ## G`。
    /// 各セクションの本文は、見出しが sticky へ収まる間隔を作るために置く。
    fn outlined_app() -> App {
        let mut source = String::new();
        for heading in ["# A", "## B", "### C", "### D", "## E", "# F", "## G"] {
            source.push_str(heading);
            source.push('\n');
            source.push_str(&"x\n".repeat(12));
        }
        // 末尾のセクションでも `max_top` に頭打ちされずに寄せきれる余白。
        source.push_str(&"x\n".repeat(24));
        app(&source, false)
    }

    fn index_of(headings: &[Heading], text: &str) -> usize {
        headings
            .iter()
            .position(|heading| heading.text == text)
            .unwrap_or_else(|| panic!("no heading named {text}"))
    }

    fn section_menu(app: &App) -> &SectionMenu {
        let Mode::Section(menu) = &app.mode else {
            panic!("the section menu should be open");
        };
        menu
    }

    /// 開いている一覧に並んでいる見出しのテキスト。
    fn menu_texts(app: &App) -> Vec<String> {
        let headings = app.active_headings();
        section_menu(app)
            .items
            .iter()
            .map(|index| headings[*index].text.clone())
            .collect()
    }

    /// 本文領域の先頭に来ている行。
    fn first_body_line(app: &App) -> usize {
        let (_, slots) = app.sticky_view();
        app.top + sticky_area_height(slots)
    }

    #[test]
    fn siblings_stay_under_the_same_parent() {
        let headings = &outlined_app().headings;
        let texts = |items: Vec<usize>| -> Vec<String> {
            items
                .into_iter()
                .map(|index| headings[index].text.clone())
                .collect()
        };

        // ## B の兄弟は同じ # A 配下のものだけで、# F 配下の ## G は入らない。
        assert_eq!(
            texts(siblings(headings, index_of(headings, "B"))),
            ["B", "E"]
        );
        assert_eq!(
            texts(siblings(headings, index_of(headings, "C"))),
            ["C", "D"]
        );
        // h1 には親が無いので文書内の全 h1 が並ぶ。
        assert_eq!(
            texts(siblings(headings, index_of(headings, "A"))),
            ["A", "F"]
        );
    }

    #[test]
    fn tab_opens_the_deepest_level_and_walks_the_chain() {
        let mut app = outlined_app();
        app.top = app.headings[index_of(&app.headings, "C")].line + 2;

        app.handle_key(press(KeyCode::Tab));
        assert_eq!(menu_texts(&app), ["C", "D"]);

        app.handle_key(press(KeyCode::BackTab));
        assert_eq!(menu_texts(&app), ["B", "E"]);

        app.handle_key(press(KeyCode::BackTab));
        assert_eq!(menu_texts(&app), ["A", "F"]);

        // 段は循環する。
        app.handle_key(press(KeyCode::BackTab));
        assert_eq!(menu_texts(&app), ["C", "D"]);
    }

    #[test]
    fn tab_at_the_top_lists_the_first_level() {
        let mut app = outlined_app();
        assert!(app.sticky_heading_lines().is_empty());

        app.handle_key(press(KeyCode::Tab));
        assert_eq!(menu_texts(&app), ["A", "F"]);

        // まだどのセクションにも入っていないので現在地の印は出ない。
        assert_eq!(section_menu(&app).current, None);
    }

    #[test]
    fn tab_does_nothing_without_headings() {
        let mut app = app("just text\n", false);

        app.handle_key(press(KeyCode::Tab));
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn selecting_previews_the_section_and_escape_rewinds() {
        let mut app = outlined_app();
        app.top = app.headings[index_of(&app.headings, "C")].line + 2;
        let origin = app.top;

        // 開いた時点の選択は今いるセクションなので、本文は動かさない。
        app.handle_key(press(KeyCode::Tab));
        assert_eq!(app.top, origin);

        app.handle_key(press(KeyCode::Down));
        let d = app.headings[index_of(&app.headings, "D")].line;
        assert_eq!(first_body_line(&app), d + 1);

        app.handle_key(press(KeyCode::Esc));
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(app.top, origin);
    }

    #[test]
    fn enter_keeps_the_previewed_position() {
        let mut app = outlined_app();
        app.top = app.headings[index_of(&app.headings, "C")].line + 2;

        app.handle_key(press(KeyCode::Tab));
        app.handle_key(press(KeyCode::Down));
        let previewed = app.top;

        app.handle_key(press(KeyCode::Enter));
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(app.top, previewed);
        // 一覧を閉じても、見出しの直後から本文が始まる位置のままでいる。
        let d = app.headings[index_of(&app.headings, "D")].line;
        assert_eq!(first_body_line(&app), d + 1);
    }

    #[test]
    fn the_menu_swallows_unbound_keys() {
        let mut app = outlined_app();
        app.top = app.headings[index_of(&app.headings, "C")].line + 2;
        app.handle_key(press(KeyCode::Tab));
        let top = app.top;

        for code in [
            KeyCode::Char('q'),
            KeyCode::Char('G'),
            KeyCode::Char('/'),
            KeyCode::Char('d'),
        ] {
            assert!(!app.handle_key(press(code)));
            assert!(matches!(app.mode, Mode::Section(_)));
            assert_eq!(app.top, top);
        }
    }

    #[test]
    fn rebuilding_the_lines_closes_the_menu() {
        let mut app = outlined_app();
        app.top = app.headings[index_of(&app.headings, "C")].line + 2;
        app.handle_key(press(KeyCode::Tab));

        app.resize(70, 24);
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn sticky_clicks_hit_the_heading_and_the_padding_above_it() {
        let mut app = outlined_app();
        app.top = app.headings[index_of(&app.headings, "C")].line + 2;
        assert_eq!(app.sticky_heading_lines().len(), 3);

        // row 2 が ## B の直上 padding、row 3 が見出しそのもの。
        for row in [2, 3] {
            app.mode = Mode::Normal;
            assert!(app.handle_click(row), "row={row}");
            assert_eq!(menu_texts(&app), ["B", "E"], "row={row}");
        }

        // 本文との境界になる最下段の padding はどの見出しにも寄せない。
        app.mode = Mode::Normal;
        assert!(!app.handle_click(6));
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn body_headings_are_clickable_too() {
        let mut app = outlined_app();
        let e = app.headings[index_of(&app.headings, "E")].line;
        app.top = e - 12;
        let row = e - app.top;
        assert!(row >= sticky_area_height(app.sticky_view().1));

        assert!(app.handle_click(row));
        assert_eq!(menu_texts(&app), ["B", "E"]);
    }

    #[test]
    fn clicking_an_item_confirms_and_clicking_elsewhere_rewinds() {
        let mut app = outlined_app();
        app.top = app.headings[index_of(&app.headings, "C")].line + 2;
        let origin = app.top;

        app.handle_key(press(KeyCode::Tab));
        // 一覧の 2 行目 = ### D。
        let area = sticky_area_height(app.sticky_view().1);
        assert!(app.handle_click(area + 1));
        assert!(matches!(app.mode, Mode::Normal));
        let d = app.headings[index_of(&app.headings, "D")].line;
        assert_eq!(first_body_line(&app), d + 1);

        app.top = origin;
        app.handle_key(press(KeyCode::Tab));
        app.handle_key(press(KeyCode::Down));
        assert_ne!(app.top, origin);

        let elsewhere = (0..app.body_height() as usize)
            .find(|row| *row > area + 2 && app.heading_at_row(*row).is_none())
            .unwrap();
        assert!(app.handle_click(elsewhere));
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(app.top, origin);
    }

    #[test]
    fn the_menu_follows_the_shown_diff_layer() {
        let mut app = outlined_app();
        app.handle_key(press(KeyCode::Char('s')));
        app.replace_source(app.source.replace("## E\n", "## E2\n"));
        app.handle_key(press(KeyCode::Char('d')));

        let c = index_of(app.active_headings(), "C");
        app.top = app.active_headings()[c].line + 2;
        app.handle_key(press(KeyCode::Tab));
        app.handle_key(press(KeyCode::BackTab));

        assert_eq!(menu_texts(&app), ["B", "E2"]);
    }

    #[test]
    fn a_short_terminal_keeps_only_the_deepest_menu_level() {
        let mut app = outlined_app();
        // body 9 行 → sticky に載せられるのは 1 段だけ。
        app.height = 10;
        app.top = app.headings[index_of(&app.headings, "C")].line + 2;

        app.handle_key(press(KeyCode::Tab));
        assert_eq!(menu_texts(&app), ["C", "D"]);
        assert_eq!(app.sticky_view().1, 1);

        // 載せられる段が 1 つしかないので階層は行き来できない。
        app.handle_key(press(KeyCode::BackTab));
        assert_eq!(menu_texts(&app), ["C", "D"]);
    }

    #[test]
    fn every_section_lands_at_the_top_of_its_body() {
        let mut app = outlined_app();
        for index in 0..app.headings.len() {
            app.mode = Mode::Normal;
            app.top = 0;
            app.open_section_menu(index);
            app.follow_section_selection();

            let line = app.headings[index].line;
            let text = app.headings[index].text.clone();
            // 見出しが sticky の最下段に載ったならその直後から、載せる余地が
            // 無かった (文書の先頭) なら見出しそのものから本文が始まる。
            let (sticky, _) = app.sticky_view();
            let expected = if sticky.last() == Some(&line) {
                line + 1
            } else {
                line
            };
            assert_eq!(first_body_line(&app), expected, "menu open at {text}");

            // 一覧を閉じても sticky の段数は変わらないので、本文の先頭も動かない。
            app.mode = Mode::Normal;
            assert_eq!(first_body_line(&app), expected, "confirmed at {text}");
        }
    }

    #[test]
    fn clicking_the_open_heading_again_closes_the_menu() {
        let mut app = outlined_app();
        app.top = app.headings[index_of(&app.headings, "C")].line + 2;
        let origin = app.top;

        // sticky は [A, B, C] で row 5 が ### C。
        assert!(app.handle_click(5));
        assert_eq!(menu_texts(&app), ["C", "D"]);
        assert!(app.handle_click(5));
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(app.top, origin);

        // 選択を動かして最下段が ### D に差し替わった後も、そこを押せば閉じる。
        app.handle_click(5);
        app.handle_key(press(KeyCode::Down));
        assert_ne!(app.top, origin);
        assert!(app.handle_click(5));
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(app.top, origin);
    }

    /// 一覧が窓に収まらない章立て。`# Doc` の下に `## S0`..`## S13`。
    fn long_outline_app() -> App {
        let mut source = String::from("# Doc\n");
        source.push_str(&"x\n".repeat(4));
        for index in 0..14 {
            source.push_str(&format!("## S{index}\n"));
            source.push_str(&"x\n".repeat(4));
        }
        app(&source, false)
    }

    #[test]
    fn the_wheel_scrolls_the_menu_window_and_leaves_the_rest_alone() {
        let mut app = long_outline_app();
        app.top = app.headings[3].line + 1;
        app.handle_key(press(KeyCode::Tab));
        let top = app.top;
        let selected = section_menu(&app).selected;
        let rows = app.section_menu_height(section_menu(&app));
        assert!(rows < section_menu(&app).items.len());

        assert!(app.handle_mouse(mouse(MouseEventKind::ScrollDown, 0, 0)));
        assert!(section_menu(&app).offset > 0);
        // 窓が動くだけで、選択も本文も動かない。
        assert_eq!(section_menu(&app).selected, selected);
        assert_eq!(app.top, top);

        // 末尾まで送ったら止まり、描き直しも要求しない。
        while app.handle_mouse(mouse(MouseEventKind::ScrollDown, 0, 0)) {}
        let menu = section_menu(&app);
        assert_eq!(menu.offset, menu.items.len() - rows);
    }

    #[test]
    fn moving_the_selection_pulls_the_window_along() {
        let mut app = long_outline_app();
        app.top = app.headings[1].line + 1;
        app.handle_key(press(KeyCode::Tab));
        let rows = app.section_menu_height(section_menu(&app));

        for _ in 0..section_menu(&app).items.len() {
            app.handle_key(press(KeyCode::Down));
            let menu = section_menu(&app);
            assert!(
                (menu.offset..menu.offset + rows).contains(&menu.selected),
                "selected={} offset={}",
                menu.selected,
                menu.offset
            );
        }
    }
}
