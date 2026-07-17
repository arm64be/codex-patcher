//! Codex-style terminal views used by the dispatcher.
//!
//! UI is always written to stderr: stdout may be an app-server or MCP protocol
//! stream. `render_*_80x12` functions are pure and contain no ANSI escapes.

use anyhow::{Context, Result};
use crossterm::cursor::{Hide, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use std::collections::VecDeque;
use std::env;
use std::io::{self, Stderr};
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

const SNAPSHOT_WIDTH: usize = 80;
const SNAPSHOT_HEIGHT: usize = 12;
const MAX_PROGRESS_LOG_LINES: usize = 16;
const PROGRESS_BOTTOM_MARGIN: usize = 3;
const PROGRESS_HEADER_LINES: usize = 6;
const PROGRESS_HISTORY_LIMIT: usize = 64;
const PROGRESS_TICK: Duration = Duration::from_secs(1);
const PROGRESS_SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
static TERMINAL_ACTIVE: AtomicBool = AtomicBool::new(false);
#[cfg(unix)]
static SIGNAL_RESTORER: OnceLock<std::result::Result<(), String>> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderOptions {
    pub color: bool,
    pub ascii: bool,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self::from_env()
    }
}

impl RenderOptions {
    pub fn from_env() -> Self {
        let term_is_dumb = env::var_os("TERM").is_some_and(|value| value == "dumb");
        Self {
            color: env::var_os("NO_COLOR").is_none() && !term_is_dumb,
            ascii: env::var_os("CODEX_PATCHER_ASCII").is_some() || term_is_dumb,
        }
    }

    pub const fn plain_ascii() -> Self {
        Self {
            color: false,
            ascii: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateScreen {
    pub current_version: String,
    pub desired_version: String,
    pub current_patch_fingerprint: Option<String>,
    pub desired_patch_fingerprint: Option<String>,
    pub release_url: Option<String>,
}

impl UpdateScreen {
    pub fn new(current_version: impl Into<String>, desired_version: impl Into<String>) -> Self {
        Self {
            current_version: current_version.into(),
            desired_version: desired_version.into(),
            current_patch_fingerprint: None,
            desired_patch_fingerprint: None,
            release_url: None,
        }
    }

    fn transition(&self) -> String {
        let versions = format!("{} -> {}", self.current_version, self.desired_version);
        match (
            self.current_patch_fingerprint.as_deref(),
            self.desired_patch_fingerprint.as_deref(),
        ) {
            (Some(current), Some(desired)) if current != desired => format!(
                "{versions}  patches {} -> {}",
                short_fingerprint(current),
                short_fingerprint(desired)
            ),
            _ => versions,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateChoice {
    Build,
    Exit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureScreen {
    pub current_version: String,
    pub desired_version: String,
    pub phase: String,
    pub summary: String,
    pub failed_patch_index: Option<usize>,
    pub failed_patch: Option<String>,
    pub log_path: PathBuf,
    pub last_good_version: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureChoice {
    Repair,
    Exit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressScreen {
    pub current_version: String,
    pub desired_version: String,
    pub phase: String,
    pub recent_lines: VecDeque<String>,
    pub log_path: Option<PathBuf>,
    pub elapsed_seconds: u64,
    pub quiet_seconds: u64,
    pub animation_frame: usize,
}

impl ProgressScreen {
    pub fn new(current_version: impl Into<String>, desired_version: impl Into<String>) -> Self {
        Self {
            current_version: current_version.into(),
            desired_version: desired_version.into(),
            phase: "Resolve upstream".to_owned(),
            recent_lines: VecDeque::new(),
            log_path: None,
            elapsed_seconds: 0,
            quiet_seconds: 0,
            animation_frame: 0,
        }
    }
}

pub fn render_update_80x12(screen: &UpdateScreen, options: RenderOptions) -> String {
    render_snapshot(update_styled_lines(screen, 0, options))
}

pub fn render_failure_80x12(screen: &FailureScreen, options: RenderOptions) -> String {
    render_snapshot(failure_styled_lines(screen, 0, options))
}

pub fn render_progress_80x12(screen: &ProgressScreen, options: RenderOptions) -> String {
    render_snapshot(progress_styled_lines(screen, SNAPSHOT_HEIGHT, options))
}

pub fn prompt_update(screen: &UpdateScreen) -> Result<UpdateChoice> {
    prompt_update_with_options(screen, RenderOptions::from_env())
}

pub fn prompt_update_with_options(
    screen: &UpdateScreen,
    options: RenderOptions,
) -> Result<UpdateChoice> {
    let mut terminal = TerminalSession::enter()?;
    let mut selected = 0;
    loop {
        terminal.draw_update(screen, selected, options)?;
        let Event::Key(key) = event::read().context("reading update prompt input")? else {
            continue;
        };
        if let Some(choice) = update_key(&mut selected, key) {
            return Ok(choice);
        }
    }
}

pub fn prompt_failure(screen: &FailureScreen) -> Result<FailureChoice> {
    prompt_failure_with_options(screen, RenderOptions::from_env())
}

pub fn prompt_failure_with_options(
    screen: &FailureScreen,
    options: RenderOptions,
) -> Result<FailureChoice> {
    let mut terminal = TerminalSession::enter()?;
    let option_count = usize::from(screen.last_good_version.is_some()) + 1;
    let mut selected = 0;
    loop {
        terminal.draw_failure(screen, selected, options)?;
        let Event::Key(key) = event::read().context("reading failure prompt input")? else {
            continue;
        };
        if let Some(choice) = failure_key(&mut selected, option_count, key) {
            return Ok(if screen.last_good_version.is_some() {
                choice
            } else {
                FailureChoice::Exit
            });
        }
    }
}

/// Cloneable build progress reporter. Updates redraw an attached display.
#[derive(Clone)]
pub struct ProgressHandle {
    shared: Arc<ProgressShared>,
}

impl std::fmt::Debug for ProgressHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProgressHandle")
            .field("screen", &self.snapshot())
            .finish_non_exhaustive()
    }
}

struct ProgressShared {
    screen: Mutex<ProgressScreen>,
    terminal: Mutex<Option<TerminalSession>>,
    options: RenderOptions,
}

impl ProgressHandle {
    pub fn detached(screen: ProgressScreen) -> Self {
        Self {
            shared: Arc::new(ProgressShared {
                screen: Mutex::new(screen),
                terminal: Mutex::new(None),
                options: RenderOptions::from_env(),
            }),
        }
    }

    pub fn set_phase(&self, phase: impl Into<String>) -> Result<()> {
        self.mutate(|screen| {
            screen.phase = sanitize_line(&phase.into());
            screen.quiet_seconds = 0;
        })
    }

    pub fn set_latest_line(&self, line: impl Into<String>) -> Result<()> {
        self.mutate(|screen| {
            if screen.recent_lines.len() == PROGRESS_HISTORY_LIMIT {
                screen.recent_lines.pop_front();
            }
            screen
                .recent_lines
                .push_back(compact_progress_line(&sanitize_line(&line.into())));
            screen.quiet_seconds = 0;
        })
    }

    pub fn clear_latest_line(&self) -> Result<()> {
        self.mutate(|screen| {
            screen.recent_lines.clear();
            screen.quiet_seconds = 0;
        })
    }

    fn tick(&self) -> Result<()> {
        self.mutate(|screen| {
            screen.elapsed_seconds = screen.elapsed_seconds.saturating_add(1);
            screen.quiet_seconds = screen.quiet_seconds.saturating_add(1);
            screen.animation_frame = screen.animation_frame.wrapping_add(1);
        })
    }

    pub fn snapshot(&self) -> ProgressScreen {
        self.shared
            .screen
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn mutate(&self, update: impl FnOnce(&mut ProgressScreen)) -> Result<()> {
        let snapshot = {
            let mut screen = self
                .shared
                .screen
                .lock()
                .map_err(|_| anyhow::anyhow!("progress state lock is poisoned"))?;
            update(&mut screen);
            screen.clone()
        };
        if let Some(terminal) = self
            .shared
            .terminal
            .lock()
            .map_err(|_| anyhow::anyhow!("progress terminal lock is poisoned"))?
            .as_mut()
        {
            terminal.draw_progress(&snapshot, self.shared.options)?;
        }
        Ok(())
    }
}

/// Owns terminal restoration for a foreground build display.
pub struct ProgressDisplay {
    shared: Arc<ProgressShared>,
    ticker_stop: Option<mpsc::Sender<()>>,
    ticker: Option<JoinHandle<()>>,
}

impl ProgressDisplay {
    pub fn start(screen: ProgressScreen) -> Result<(Self, ProgressHandle)> {
        Self::start_with_options(screen, RenderOptions::from_env())
    }

    pub fn start_with_options(
        screen: ProgressScreen,
        options: RenderOptions,
    ) -> Result<(Self, ProgressHandle)> {
        // Progress does not consume keys. Keeping canonical terminal input
        // means Ctrl+C remains a real console signal delivered to both the
        // manager and the upstream builder process tree.
        let mut terminal = TerminalSession::enter_progress()?;
        terminal.draw_progress(&screen, options)?;
        let shared = Arc::new(ProgressShared {
            screen: Mutex::new(screen),
            terminal: Mutex::new(Some(terminal)),
            options,
        });
        let (ticker_stop, ticker_wait) = mpsc::channel();
        let ticker_handle = ProgressHandle {
            shared: Arc::clone(&shared),
        };
        let ticker = std::thread::Builder::new()
            .name("codex-patcher-progress".to_owned())
            .spawn(move || {
                while matches!(
                    ticker_wait.recv_timeout(PROGRESS_TICK),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ) {
                    if ticker_handle.tick().is_err() {
                        break;
                    }
                }
            })
            .context("starting build progress refresh")?;
        Ok((
            Self {
                shared: Arc::clone(&shared),
                ticker_stop: Some(ticker_stop),
                ticker: Some(ticker),
            },
            ProgressHandle { shared },
        ))
    }
}

impl Drop for ProgressDisplay {
    fn drop(&mut self) {
        if let Some(stop) = self.ticker_stop.take() {
            let _ = stop.send(());
        }
        if let Some(ticker) = self.ticker.take() {
            let _ = ticker.join();
        }
        if let Ok(mut terminal) = self.shared.terminal.lock() {
            // Restore immediately even if cloned handles outlive this owner.
            let _ = terminal.take();
        }
    }
}

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stderr>>,
    raw: bool,
}

impl TerminalSession {
    fn enter() -> Result<Self> {
        Self::enter_mode(true)
    }

    fn enter_progress() -> Result<Self> {
        Self::enter_mode(false)
    }

    fn enter_mode(raw: bool) -> Result<Self> {
        install_signal_restorer()?;
        TERMINAL_ACTIVE.store(true, Ordering::SeqCst);
        if raw && let Err(error) = enable_raw_mode() {
            TERMINAL_ACTIVE.store(false, Ordering::SeqCst);
            return Err(error).context("enabling terminal raw mode");
        }
        let mut stderr = io::stderr();
        if let Err(error) = execute!(stderr, EnterAlternateScreen, Hide) {
            if raw {
                let _ = disable_raw_mode();
            }
            TERMINAL_ACTIVE.store(false, Ordering::SeqCst);
            return Err(error).context("entering alternate screen");
        }
        match Terminal::new(CrosstermBackend::new(stderr)) {
            Ok(terminal) => Ok(Self { terminal, raw }),
            Err(error) => {
                let mut stderr = io::stderr();
                let _ = execute!(stderr, Show, LeaveAlternateScreen);
                if raw {
                    let _ = disable_raw_mode();
                }
                TERMINAL_ACTIVE.store(false, Ordering::SeqCst);
                Err(error).context("creating terminal renderer")
            }
        }
    }

    fn draw_update(
        &mut self,
        screen: &UpdateScreen,
        selected: usize,
        options: RenderOptions,
    ) -> Result<()> {
        self.draw_lines(
            update_styled_lines(screen, selected, options),
            "drawing update prompt",
        )
    }

    fn draw_failure(
        &mut self,
        screen: &FailureScreen,
        selected: usize,
        options: RenderOptions,
    ) -> Result<()> {
        self.draw_lines(
            failure_styled_lines(screen, selected, options),
            "drawing failure prompt",
        )
    }

    fn draw_progress(&mut self, screen: &ProgressScreen, options: RenderOptions) -> Result<()> {
        self.terminal
            .draw(|frame| {
                let area = frame.area();
                let lines = progress_styled_lines(screen, usize::from(area.height), options);
                frame.render_widget(Clear, area);
                frame.render_widget(Paragraph::new(lines), area);
            })
            .context("drawing build progress")?;
        Ok(())
    }

    fn draw_lines(&mut self, lines: Vec<Line<'static>>, context: &'static str) -> Result<()> {
        self.terminal
            .draw(|frame| {
                frame.render_widget(Clear, frame.area());
                frame.render_widget(Paragraph::new(lines), frame.area());
            })
            .context(context)?;
        Ok(())
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = self.terminal.show_cursor();
        let _ = execute!(self.terminal.backend_mut(), Show, LeaveAlternateScreen);
        if self.raw {
            let _ = disable_raw_mode();
        }
        TERMINAL_ACTIVE.store(false, Ordering::SeqCst);
    }
}

#[cfg(unix)]
fn install_signal_restorer() -> Result<()> {
    let result = SIGNAL_RESTORER.get_or_init(|| {
        use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
        use signal_hook::iterator::Signals;

        let mut signals =
            Signals::new([SIGHUP, SIGINT, SIGQUIT, SIGTERM]).map_err(|error| error.to_string())?;
        std::thread::Builder::new()
            .name("codex-patcher-terminal-signals".to_owned())
            .spawn(move || {
                for signal in signals.forever() {
                    if TERMINAL_ACTIVE.swap(false, Ordering::SeqCst) {
                        restore_terminal_best_effort();
                    }
                    if signal_hook::low_level::emulate_default_handler(signal).is_err() {
                        std::process::exit(128 + signal);
                    }
                }
            })
            .map(|_| ())
            .map_err(|error| error.to_string())
    });
    result
        .as_ref()
        .map(|_| ())
        .map_err(|error| anyhow::anyhow!(error.clone()))
}

#[cfg(not(unix))]
fn install_signal_restorer() -> Result<()> {
    // Windows Ctrl+C/Ctrl+D arrive as crossterm key events while raw mode is
    // active. Console-close events tear down the console itself, so there is
    // no persistent terminal mode to restore.
    Ok(())
}

#[cfg(unix)]
fn restore_terminal_best_effort() {
    let mut stderr = io::stderr();
    let _ = execute!(stderr, Show, LeaveAlternateScreen);
    let _ = disable_raw_mode();
}

fn update_key(selected: &mut usize, key: KeyEvent) -> Option<UpdateChoice> {
    menu_key(selected, 2, key).map(|choice| match choice {
        MenuChoice::Select(0) => UpdateChoice::Build,
        MenuChoice::Select(_) | MenuChoice::Exit => UpdateChoice::Exit,
    })
}

fn failure_key(selected: &mut usize, option_count: usize, key: KeyEvent) -> Option<FailureChoice> {
    menu_key(selected, option_count, key).map(|choice| match choice {
        MenuChoice::Select(0) if option_count == 2 => FailureChoice::Repair,
        MenuChoice::Select(_) | MenuChoice::Exit => FailureChoice::Exit,
    })
}

#[derive(Debug, PartialEq, Eq)]
enum MenuChoice {
    Select(usize),
    Exit,
}

fn menu_key(selected: &mut usize, option_count: usize, key: KeyEvent) -> Option<MenuChoice> {
    if matches!(key.kind, KeyEventKind::Release) {
        return None;
    }
    if key.code == KeyCode::Esc
        || (key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('d')))
    {
        return Some(MenuChoice::Exit);
    }
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            *selected = (*selected + option_count - 1) % option_count
        }
        KeyCode::Down | KeyCode::Char('j') => *selected = (*selected + 1) % option_count,
        KeyCode::Char(number) => {
            let index = number.to_digit(10)? as usize;
            return (index > 0 && index <= option_count).then(|| MenuChoice::Select(index - 1));
        }
        KeyCode::Enter => return Some(MenuChoice::Select(*selected)),
        _ => {}
    }
    None
}

fn update_styled_lines(
    screen: &UpdateScreen,
    selected: usize,
    options: RenderOptions,
) -> Vec<Line<'static>> {
    let cyan = selected_style(options);
    let marker = marker(options);
    let icon = update_icon(options);
    let link = screen
        .release_url
        .as_deref()
        .unwrap_or("https://github.com/openai/codex/releases");
    vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(format!("  {icon} "), cyan.add_modifier(Modifier::BOLD)),
            Span::styled(
                "Update available!",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(
                screen.transition(),
                Style::default().add_modifier(Modifier::DIM),
            ),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "  Release notes: ",
                Style::default().add_modifier(Modifier::DIM),
            ),
            Span::styled(
                link.to_owned(),
                Style::default().add_modifier(Modifier::DIM | Modifier::UNDERLINED),
            ),
        ]),
        Line::from(""),
        selection_line(marker, 1, "Build patched update now", selected == 0, cyan),
        selection_line(marker, 2, "Exit", selected == 1, cyan),
        Line::from(""),
        Line::from(vec![
            Span::styled("  Press ", Style::default().add_modifier(Modifier::DIM)),
            Span::raw("Enter"),
            Span::styled(" to continue", Style::default().add_modifier(Modifier::DIM)),
        ]),
    ]
}

fn failure_styled_lines(
    screen: &FailureScreen,
    selected: usize,
    options: RenderOptions,
) -> Vec<Line<'static>> {
    let cyan = selected_style(options);
    let marker = marker(options);
    let icon = if options.ascii { "!" } else { "⚠" };
    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(format!("  {icon} "), cyan.add_modifier(Modifier::BOLD)),
            Span::styled(
                "Patched update failed",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(
                format!("{} -> {}", screen.current_version, screen.desired_version),
                Style::default().add_modifier(Modifier::DIM),
            ),
        ]),
        Line::from(""),
        Line::from(format!("  Phase: {}", sanitize_line(&screen.phase))),
    ];
    if let Some(patch) = &screen.failed_patch {
        let label = screen
            .failed_patch_index
            .map_or_else(|| "Patch".into(), |index| format!("Patch {index}"));
        lines.push(Line::from(format!("  {label}: {}", sanitize_line(patch))));
    }
    lines.push(Line::from(format!("  {}", sanitize_line(&screen.summary))));
    lines.push(Line::from(vec![
        Span::styled("  Log: ", Style::default().add_modifier(Modifier::DIM)),
        Span::styled(
            screen.log_path.display().to_string(),
            Style::default().add_modifier(Modifier::DIM),
        ),
    ]));
    lines.push(Line::from(""));
    if let Some(version) = &screen.last_good_version {
        lines.push(selection_line(
            marker,
            1,
            &format!("Repair with last-good Codex {version}"),
            selected == 0,
            cyan,
        ));
        lines.push(selection_line(marker, 2, "Exit", selected == 1, cyan));
    } else {
        lines.push(selection_line(marker, 1, "Exit", true, cyan));
    }
    lines
}

fn progress_styled_lines(
    screen: &ProgressScreen,
    height: usize,
    options: RenderOptions,
) -> Vec<Line<'static>> {
    let green = cargo_style(options, Color::Green);
    let icon = update_icon(options);
    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(format!("  {icon} "), green.add_modifier(Modifier::BOLD)),
            Span::styled(
                "Building patched Codex",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(
                format!("{} -> {}", screen.current_version, screen.desired_version),
                Style::default().add_modifier(Modifier::DIM),
            ),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  Phase  ", Style::default().add_modifier(Modifier::DIM)),
            Span::styled(
                sanitize_line(&screen.phase),
                green.add_modifier(Modifier::BOLD),
            ),
        ]),
        progress_activity_line(screen, options),
        Line::from(""),
    ];
    let capacity = progress_log_capacity(height);
    if capacity > 0 {
        if screen.recent_lines.is_empty() {
            lines.push(Line::from(Span::styled(
                "  Waiting for build output…",
                Style::default().add_modifier(Modifier::DIM),
            )));
        } else {
            lines.extend(
                screen
                    .recent_lines
                    .iter()
                    .skip(screen.recent_lines.len().saturating_sub(capacity))
                    .map(|line| progress_log_line(line, options)),
            );
        }
    }
    lines
}

fn progress_activity_line(screen: &ProgressScreen, options: RenderOptions) -> Line<'static> {
    let spinner = if options.ascii {
        ["|", "/", "-", "\\"][screen.animation_frame % 4]
    } else {
        PROGRESS_SPINNER[screen.animation_frame % PROGRESS_SPINNER.len()]
    };
    let mut spans = vec![
        Span::styled(
            format!("  {spinner} "),
            cargo_style(options, Color::Green).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "Still working",
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" · {} elapsed", concise_duration(screen.elapsed_seconds)),
            Style::default().add_modifier(Modifier::DIM),
        ),
    ];
    if !screen.recent_lines.is_empty() && screen.quiet_seconds >= 2 {
        spans.push(Span::styled(
            format!(
                " · last output {} ago",
                concise_duration(screen.quiet_seconds)
            ),
            Style::default().add_modifier(Modifier::DIM),
        ));
    }
    Line::from(spans)
}

fn progress_log_capacity(height: usize) -> usize {
    height
        .saturating_sub(PROGRESS_HEADER_LINES + PROGRESS_BOTTOM_MARGIN)
        .min(MAX_PROGRESS_LOG_LINES)
}

fn progress_log_line(value: &str, options: RenderOptions) -> Line<'static> {
    let value = sanitize_line(value);
    let trimmed = value.trim_start();
    for (prefix, color) in [
        ("Compiling", Color::Green),
        ("Checking", Color::Green),
        ("Finished", Color::Green),
        ("Running", Color::Green),
        ("Updating", Color::Green),
        ("Downloading", Color::Green),
        ("Downloaded", Color::Green),
        ("Locking", Color::Green),
        ("Adding", Color::Green),
        ("Removing", Color::Red),
        ("Downgrading", Color::Yellow),
    ] {
        if let Some(rest) = trimmed.strip_prefix(prefix)
            && (rest.is_empty() || rest.starts_with(char::is_whitespace))
        {
            return Line::from(vec![
                Span::styled(
                    format!("  {prefix:>11}"),
                    cargo_style(options, color).add_modifier(Modifier::BOLD),
                ),
                Span::raw(rest.to_owned()),
            ]);
        }
    }

    if let Some(separator) = trimmed.find(':') {
        let label = &trimmed[..=separator];
        let color = if label.starts_with("warning") {
            Some(Color::Yellow)
        } else if label.starts_with("error") {
            Some(Color::Red)
        } else if matches!(label, "help:" | "note:") {
            Some(Color::Cyan)
        } else {
            None
        };
        if let Some(color) = color {
            return Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    label.to_owned(),
                    cargo_style(options, color).add_modifier(Modifier::BOLD),
                ),
                Span::raw(trimmed[separator + 1..].to_owned()),
            ]);
        }
    }

    if trimmed.starts_with("-->") {
        return Line::from(vec![
            Span::raw("  "),
            Span::styled(trimmed.to_owned(), cargo_style(options, Color::Cyan)),
        ]);
    }
    if let Some(gutter) = value.find('|') {
        return Line::from(vec![
            Span::raw("  "),
            Span::styled(
                value[..=gutter].to_owned(),
                cargo_style(options, Color::Cyan),
            ),
            Span::raw(value[gutter + 1..].to_owned()),
        ]);
    }
    Line::from(format!("  {value}"))
}

fn cargo_style(options: RenderOptions, color: Color) -> Style {
    if options.color {
        Style::default().fg(color)
    } else {
        Style::default()
    }
}

fn concise_duration(seconds: u64) -> String {
    let hours = seconds / 3_600;
    let minutes = seconds % 3_600 / 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours}h {minutes}m {seconds}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

fn compact_progress_line(value: &str) -> String {
    let trimmed = value.trim_start();
    let cargo_status = [
        "Compiling",
        "Checking",
        "Finished",
        "Running",
        "Updating",
        "Downloading",
        "Downloaded",
    ]
    .iter()
    .any(|prefix| trimmed.starts_with(prefix));
    if cargo_status
        && trimmed.ends_with(')')
        && let Some(separator) = trimmed.rfind(" (")
        && Path::new(&trimmed[separator + 2..trimmed.len() - 1]).is_absolute()
    {
        return trimmed[..separator].to_owned();
    }
    value.to_owned()
}

fn selection_line(
    marker: &str,
    number: usize,
    label: &str,
    selected: bool,
    style: Style,
) -> Line<'static> {
    if selected {
        Line::from(vec![
            Span::styled(marker.to_owned(), style),
            Span::raw(format!(" {number}. {label}")),
        ])
    } else {
        Line::from(format!("  {number}. {label}"))
    }
}

fn selected_style(options: RenderOptions) -> Style {
    if options.color {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    }
}

fn marker(options: RenderOptions) -> &'static str {
    if options.ascii { ">" } else { "›" }
}

fn update_icon(options: RenderOptions) -> &'static str {
    if options.ascii { "*" } else { "✨" }
}

fn render_snapshot(lines: Vec<Line<'static>>) -> String {
    let mut rows = Vec::with_capacity(SNAPSHOT_HEIGHT);
    for line in lines.into_iter().take(SNAPSHOT_HEIGHT) {
        rows.push(fit_to_width(&line.to_string(), SNAPSHOT_WIDTH));
    }
    while rows.len() < SNAPSHOT_HEIGHT {
        rows.push(" ".repeat(SNAPSHOT_WIDTH));
    }
    rows.join("\n")
}

fn fit_to_width(value: &str, width: usize) -> String {
    let mut result = String::new();
    let mut used = 0;
    for character in value.chars() {
        let character_width = display_width(character);
        if used + character_width > width {
            break;
        }
        result.push(character);
        used += character_width;
    }
    result.push_str(&" ".repeat(width.saturating_sub(used)));
    result
}

fn display_width(character: char) -> usize {
    match character {
        '✨' => 2,
        _ => 1,
    }
}

fn sanitize_line(value: &str) -> String {
    value
        .chars()
        .map(|character| match character {
            '\r' | '\n' | '\t' | '\u{1b}' => ' ',
            character if character.is_control() => ' ',
            character => character,
        })
        .collect()
}

fn short_fingerprint(value: &str) -> &str {
    value.get(..value.len().min(8)).unwrap_or(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update_screen() -> UpdateScreen {
        UpdateScreen {
            current_version: "0.144.0".into(),
            desired_version: "0.145.0".into(),
            current_patch_fingerprint: Some("111111111111".into()),
            desired_patch_fingerprint: Some("222222222222".into()),
            release_url: Some("https://github.com/openai/codex/releases/tag/rust-v0.145.0".into()),
        }
    }

    #[test]
    fn update_snapshot_is_exactly_80_by_12_and_matches_visual_grammar() {
        let mut screen = update_screen();
        let rendered = render_update_80x12(&screen, RenderOptions::plain_ascii());
        let rows: Vec<_> = rendered.lines().collect();
        assert_eq!(rows.len(), 12);
        assert!(rows.iter().all(|row| row.chars().count() == 80));
        assert_eq!(
            rows[1].trim_end(),
            "  * Update available! 0.144.0 -> 0.145.0  patches 11111111 -> 22222222"
        );
        assert_eq!(rows[5].trim_end(), "> 1. Build patched update now");
        assert_eq!(rows[6].trim_end(), "  2. Exit");

        screen.desired_version = screen.current_version.clone();
        let patch_only = render_update_80x12(
            &screen,
            RenderOptions {
                color: false,
                ascii: false,
            },
        );
        assert!(
            patch_only.contains("0.144.0 -> 0.144.0")
                && patch_only.contains("patches")
                && patch_only.contains('✨')
                && !patch_only.contains('\u{1b}')
        );
        screen.desired_patch_fingerprint = screen.current_patch_fingerprint.clone();
        assert!(!render_update_80x12(&screen, RenderOptions::plain_ascii()).contains("patches"));
    }

    #[test]
    fn all_documented_update_keys_work_and_release_is_ignored() {
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        for code in [
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Char('j'),
            KeyCode::Char('k'),
        ] {
            let mut selected = 0;
            assert_eq!(menu_key(&mut selected, 2, key(code)), None);
            assert_eq!(selected, 1);
        }

        let mut selected = 1;
        assert_eq!(
            update_key(&mut selected, key(KeyCode::Enter)),
            Some(UpdateChoice::Exit)
        );
        assert_eq!(
            update_key(&mut selected, key(KeyCode::Char('1'))),
            Some(UpdateChoice::Build)
        );
        assert_eq!(
            update_key(&mut selected, key(KeyCode::Char('2'))),
            Some(UpdateChoice::Exit)
        );
        for code in [KeyCode::Esc, KeyCode::Char('c'), KeyCode::Char('d')] {
            let modifiers = if code == KeyCode::Esc {
                KeyModifiers::NONE
            } else {
                KeyModifiers::CONTROL
            };
            assert_eq!(
                update_key(&mut selected, KeyEvent::new(code, modifiers)),
                Some(UpdateChoice::Exit)
            );
        }
        let released = KeyEvent::new_with_kind(
            KeyCode::Char('1'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        );
        assert_eq!(update_key(&mut selected, released), None);

        assert_eq!(
            failure_key(&mut selected, 2, key(KeyCode::Char('1'))),
            Some(FailureChoice::Repair)
        );
        assert_eq!(
            failure_key(&mut selected, 2, key(KeyCode::Char('2'))),
            Some(FailureChoice::Exit)
        );
        assert_eq!(
            failure_key(&mut selected, 1, key(KeyCode::Char('1'))),
            Some(FailureChoice::Exit)
        );
    }

    #[test]
    fn progress_handle_sanitizes_updates_and_can_run_detached() {
        let handle = ProgressHandle::detached(ProgressScreen::new("0.1", "0.2"));
        handle.set_phase("Apply\npatches").unwrap();
        handle.set_latest_line("patch 1\rfailed").unwrap();
        handle.tick().unwrap();
        let snapshot = handle.snapshot();
        assert_eq!(snapshot.phase, "Apply patches");
        assert_eq!(
            snapshot.recent_lines.back().map(String::as_str),
            Some("patch 1 failed")
        );
        assert_eq!(snapshot.elapsed_seconds, 1);
        assert_eq!(snapshot.quiet_seconds, 1);
        let rendered = render_progress_80x12(&snapshot, RenderOptions::plain_ascii());
        assert!(rendered.contains("Phase  Apply patches"));
        assert!(rendered.contains("Still working · 1s elapsed"));
    }

    #[test]
    fn progress_view_keeps_a_bounded_tail_above_the_bottom_margin() {
        assert_eq!(progress_log_capacity(8), 0);
        assert_eq!(progress_log_capacity(12), 3);
        assert_eq!(progress_log_capacity(25), 16);
        assert_eq!(progress_log_capacity(80), 16);

        let handle = ProgressHandle::detached(ProgressScreen::new("0.1", "0.2"));
        for index in 0..PROGRESS_HISTORY_LIMIT + 2 {
            handle.set_latest_line(format!("line {index}")).unwrap();
        }
        let snapshot = handle.snapshot();
        assert_eq!(snapshot.recent_lines.len(), PROGRESS_HISTORY_LIMIT);
        assert_eq!(snapshot.recent_lines.front().unwrap(), "line 2");
        let rendered = render_progress_80x12(&snapshot, RenderOptions::plain_ascii());
        assert!(!rendered.contains("line 62"));
        assert!(rendered.contains("line 63"));
        assert!(rendered.contains("line 64"));
        assert!(rendered.contains("line 65"));
    }

    #[test]
    fn progress_lines_use_cargo_colors_and_hide_ephemeral_checkout_paths() {
        let options = RenderOptions {
            color: true,
            ascii: true,
        };
        let compiling = progress_log_line("   Compiling rcgen v0.14.7", options);
        assert_eq!(compiling.spans[0].style.fg, Some(Color::Green));
        let warning = progress_log_line("warning: generated one warning", options);
        assert_eq!(warning.spans[1].style.fg, Some(Color::Yellow));
        let error = progress_log_line("error[E0463]: missing core", options);
        assert_eq!(error.spans[1].style.fg, Some(Color::Red));

        #[cfg(unix)]
        assert_eq!(
            compact_progress_line("   Compiling codex-core v0.144.5 (/tmp/worktree/codex-rs/core)"),
            "Compiling codex-core v0.144.5"
        );
    }

    #[test]
    fn failure_without_last_good_only_offers_exit() {
        let screen = FailureScreen {
            current_version: "0.1".into(),
            desired_version: "0.2".into(),
            phase: "Apply patches".into(),
            summary: "patch did not apply".into(),
            failed_patch_index: None,
            failed_patch: Some("0001.patch".into()),
            log_path: PathBuf::from("/tmp/build.log"),
            last_good_version: None,
        };
        let rendered = render_failure_80x12(&screen, RenderOptions::plain_ascii());
        assert!(rendered.contains("> 1. Exit"));
        assert!(!rendered.contains("Repair with"));
    }
}
