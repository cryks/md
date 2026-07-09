use std::{
    io::{self, Write},
    path::PathBuf,
    time::Duration,
};

use anyhow::Result;
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
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

pub(crate) fn run(path: PathBuf, source: String) -> Result<()> {
    let mut terminal = TerminalSession::enter()?;
    let (width, height) = terminal::size()?;
    let mut app = App::new(path, source, width, height);
    app.draw(terminal.writer())?;

    loop {
        if event::poll(Duration::from_millis(250))? {
            match event::read()? {
                Event::Key(key) if app.handle_key(key) => break,
                Event::Resize(width, height) => app.resize(width, height),
                _ => {}
            }
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
}

impl App {
    fn new(path: PathBuf, source: String, width: u16, height: u16) -> Self {
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

        format!("{filename}  {percent:>3}%{query}{column}  q:quit  /:search")
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
