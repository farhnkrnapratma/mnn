use clap::{ArgGroup, CommandFactory, Parser, ValueEnum};
use clap_complete::{Shell, generate};
use crossterm::terminal;
use serde_json::Value;
use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fmt::Display;
use std::fs::{self, File, OpenOptions};
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

const NUSHELL_COMPLETION: &str = include_str!("../resources/mnn.nu");
const MAN_PAGE: &str = include_str!("../resources/mnn.1");

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, ValueEnum)]
enum Target {
    Auto,
    All,
    Debian,
    Apt,
    Apk,
    Aur,
    Brew,
    Bun,
    Choco,
    Determinate,
    Dnf,
    Emerge,
    Eopkg,
    Flatpak,
    Pacman,
    Pip,
    Pkg,
    Pnpm,
    #[value(name = "rpm-ostree")]
    RpmOstree,
    Snap,
    Soar,
    #[value(name = "transactional-update")]
    TransactionalUpdate,
    Uv,
    Winget,
    Xbps,
    Yum,
    Zypper,
}

impl Target {
    fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::All => "all",
            Self::Debian => "debian",
            Self::Apt => "apt",
            Self::Apk => "apk",
            Self::Aur => "aur",
            Self::Brew => "brew",
            Self::Bun => "bun",
            Self::Choco => "choco",
            Self::Determinate => "determinate",
            Self::Dnf => "dnf",
            Self::Emerge => "emerge",
            Self::Eopkg => "eopkg",
            Self::Flatpak => "flatpak",
            Self::Pacman => "pacman",
            Self::Pip => "pip",
            Self::Pkg => "pkg",
            Self::Pnpm => "pnpm",
            Self::RpmOstree => "rpm-ostree",
            Self::Snap => "snap",
            Self::Soar => "soar",
            Self::TransactionalUpdate => "transactional-update",
            Self::Uv => "uv",
            Self::Winget => "winget",
            Self::Xbps => "xbps",
            Self::Yum => "yum",
            Self::Zypper => "zypper",
        }
    }

    fn all() -> Vec<Self> {
        vec![
            Self::Apk,
            Self::Apt,
            Self::Aur,
            Self::Brew,
            Self::Bun,
            Self::Choco,
            Self::Determinate,
            Self::Dnf,
            Self::Emerge,
            Self::Eopkg,
            Self::Flatpak,
            Self::Pacman,
            Self::Pip,
            Self::Pkg,
            Self::Pnpm,
            Self::RpmOstree,
            Self::Snap,
            Self::Soar,
            Self::TransactionalUpdate,
            Self::Uv,
            Self::Winget,
            Self::Xbps,
            Self::Yum,
            Self::Zypper,
        ]
    }

    fn needs_sudo(self) -> bool {
        #[cfg(windows)]
        {
            false
        }
        #[cfg(not(windows))]
        {
            !is_termux()
                && matches!(
                    self,
                    Self::Apt
                        | Self::Apk
                        | Self::Dnf
                        | Self::Yum
                        | Self::Zypper
                        | Self::Pacman
                        | Self::Xbps
                        | Self::RpmOstree
                        | Self::TransactionalUpdate
                        | Self::Emerge
                        | Self::Eopkg
                        | Self::Pkg
                        | Self::Snap
                        | Self::Determinate
                )
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum CompletionMode {
    Bash,
    Fish,
    Zsh,
    Nu,
    All,
    Detect,
}

#[derive(Parser, Debug)]
#[command(
    name = "mnn",
    version,
    about = "Safely update package managers from one cross-platform CLI",
    group(ArgGroup::new("maintenance").multiple(false).args([
        "install",
        "reinstall",
        "update",
        "uninstall",
        "purge",
        "gen_config"
    ]))
)]
struct Cli {
    /// Show commands without executing them
    #[arg(short = 'n', long)]
    dry_run: bool,
    /// Hide package-manager output
    #[arg(short = 's', long, conflicts_with = "verbose")]
    silent: bool,
    /// Show commands and all package-manager output
    #[arg(short = 'v', long, conflicts_with = "silent")]
    verbose: bool,
    /// Run user-level targets alongside system targets
    #[arg(long, conflicts_with = "no_parallel")]
    parallel: bool,
    /// Run all targets sequentially
    #[arg(short = 'P', long, conflicts_with = "parallel")]
    no_parallel: bool,
    /// Disable file logging
    #[arg(long)]
    disable_log: bool,
    /// Generate a default configuration file
    #[arg(short = 'c', long)]
    gen_config: bool,
    /// Install completions for one or more shells, or detect installed shells
    #[arg(long, value_enum, num_args = 0..=1, default_missing_value = "detect")]
    install_completions: Option<CompletionMode>,
    /// Print a completion script instead of installing it
    #[arg(long, value_enum, conflicts_with = "install_completions")]
    print_completion: Option<CompletionMode>,
    /// Install this executable and its completions
    #[arg(short = 'i', long)]
    install: bool,
    /// Reinstall this executable and selected completions
    #[arg(short = 'r', long)]
    reinstall: bool,
    /// Update the installed executable and completions
    #[arg(short = 'u', long)]
    update: bool,
    /// Remove the installed executable and completions
    #[arg(short = 'x', long)]
    uninstall: bool,
    /// Remove the executable, completions, config, and logs
    #[arg(short = 'p', long)]
    purge: bool,
    /// Package managers to update (defaults to auto)
    #[arg(value_enum)]
    targets: Vec<Target>,
}

#[derive(Clone, Debug)]
struct Settings {
    targets: Vec<Target>,
    silent: bool,
    verbose: bool,
    parallel: bool,
    disable_log: bool,
}

#[derive(Clone, Debug)]
struct Step {
    description: String,
    program: String,
    args: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
    system: bool,
}

impl Step {
    fn new(description: &str, program: &str, args: &[&str], system: bool) -> Self {
        Self {
            description: description.to_owned(),
            program: program.to_owned(),
            args: args.iter().map(OsString::from).collect(),
            env: Vec::new(),
            system,
        }
    }

    fn with_env(mut self, env: &[(&str, &str)]) -> Self {
        self.env =
            env.iter().map(|(key, value)| (OsString::from(key), OsString::from(value))).collect();
        self
    }
}

#[derive(Clone, Debug)]
enum Status {
    Ok,
    Unavailable(String),
    Skipped(String),
    Failed(String),
}

#[derive(Clone, Debug)]
struct TargetResult {
    target: Target,
    status: Status,
    output: Vec<String>,
}

type SharedLog = Option<Arc<Mutex<File>>>;

struct SudoKeepalive {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl SudoKeepalive {
    fn start(sudo: PathBuf) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                std::thread::park_timeout(Duration::from_secs(50));
                if thread_stop.load(Ordering::Acquire) {
                    break;
                }
                let _ = Command::new(&sudo)
                    .args(["-n", "-v"])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
            }
        });
        Self { stop, thread: Some(thread) }
    }
}

impl Drop for SudoKeepalive {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let thread = self.thread.take().expect("the keepalive thread is joined once");
        thread.thread().unpark();
        let _ = thread.join();
    }
}

#[derive(Clone)]
struct ProgressBar {
    inner: Arc<ProgressInner>,
    index: usize,
}

struct ProgressInner {
    rows: Mutex<Vec<ProgressRow>>,
    visible: bool,
    footer: bool,
    top: bool,
    top_offset: usize,
    colored: bool,
    width: usize,
    height: usize,
    stop: AtomicBool,
}

struct ProgressRow {
    message: String,
    symbol: char,
    status: &'static str,
    running: bool,
}

fn describe<E: Display>(context: impl Display) -> impl FnOnce(E) -> String {
    move |error| format!("{context}: {error}")
}

fn missing(message: &'static str) -> impl FnOnce() -> String {
    move || message.to_owned()
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

static CAPTURE: Mutex<Option<Capture>> = Mutex::new(None);

#[derive(Default)]
struct Capture {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    fail: bool,
}

struct Console;

impl Write for Console {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        emit(false, bytes).map(|()| bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn emit(is_stderr: bool, bytes: &[u8]) -> io::Result<()> {
    if let Some(capture) = lock(&CAPTURE).as_mut() {
        if capture.fail {
            return Err(io::Error::other("console unavailable"));
        }
        let sink = if is_stderr { &mut capture.stderr } else { &mut capture.stdout };
        sink.extend_from_slice(bytes);
        return Ok(());
    }
    if is_stderr {
        let mut stream = io::stderr().lock();
        stream.write_all(bytes)?;
        stream.flush()
    } else {
        let mut stream = io::stdout().lock();
        stream.write_all(bytes)?;
        stream.flush()
    }
}

const PROGRESS_FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

struct ProgressManager {
    inner: Arc<ProgressInner>,
    ticker: Option<JoinHandle<()>>,
    footer_active: bool,
}

impl ProgressManager {
    fn new(show_progress: bool, silent: bool, top_offset: usize, targets: &[Target]) -> Self {
        let size = terminal::size().unwrap_or((80, 24));
        Self::with_size(show_progress, silent, top_offset, targets, size)
    }

    fn with_size(
        show_progress: bool,
        silent: bool,
        top_offset: usize,
        targets: &[Target],
        (width, height): (u16, u16),
    ) -> Self {
        let footer = use_progress_footer(show_progress, height as usize, targets.len(), top_offset);
        let visible = footer || (show_progress && silent);
        let inner = Arc::new(ProgressInner {
            rows: Mutex::new(
                targets
                    .iter()
                    .map(|target| ProgressRow {
                        message: format!("Waiting for {}...", target.name()),
                        symbol: ' ',
                        status: "WAITING",
                        running: false,
                    })
                    .collect(),
            ),
            visible,
            footer,
            top: silent,
            top_offset,
            colored: show_progress && std::env::var_os("NO_COLOR").is_none(),
            width: width as usize,
            height: height as usize,
            stop: AtomicBool::new(false),
        });
        if footer {
            initialize_footer(&inner);
        }
        let ticker = visible.then(|| {
            let thread_inner = Arc::clone(&inner);
            std::thread::spawn(move || {
                let mut frame = 0;
                while !thread_inner.stop.load(Ordering::Acquire) {
                    std::thread::park_timeout(Duration::from_millis(100));
                    if thread_inner.stop.load(Ordering::Acquire) {
                        break;
                    }
                    let mut rows = lock(&thread_inner.rows);
                    let mut changed = false;
                    for row in rows.iter_mut().filter(|row| row.running) {
                        row.symbol = PROGRESS_FRAMES[frame % PROGRESS_FRAMES.len()];
                        changed = true;
                    }
                    frame = frame.wrapping_add(1);
                    if changed {
                        draw_footer(&thread_inner, &rows);
                    }
                }
            })
        });
        Self { inner, ticker, footer_active: footer }
    }

    fn bar(&self, index: usize) -> ProgressBar {
        ProgressBar { inner: Arc::clone(&self.inner), index }
    }

    fn finish(&mut self) {
        self.inner.stop.store(true, Ordering::Release);
        if let Some(thread) = self.ticker.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
        if self.footer_active {
            teardown_footer(&self.inner);
            self.footer_active = false;
        } else if self.inner.top && self.inner.visible {
            finish_top_progress(&self.inner);
        }
    }
}

fn use_progress_footer(
    show_progress: bool,
    height: usize,
    target_count: usize,
    top_offset: usize,
) -> bool {
    show_progress && height >= target_count + top_offset + 4
}

impl Drop for ProgressManager {
    fn drop(&mut self) {
        self.finish();
    }
}

impl ProgressBar {
    fn set_message(&self, message: &str) {
        let mut rows = lock(&self.inner.rows);
        if let Some(row) = rows.get_mut(self.index) {
            row.message = message.to_owned();
            row.symbol = PROGRESS_FRAMES[0];
            row.status = "UPDATING";
            row.running = true;
            draw_footer(&self.inner, &rows);
        }
    }

    fn finish(&self, target: Target, status: &Status) {
        let mut rows = lock(&self.inner.rows);
        if let Some(row) = rows.get_mut(self.index) {
            match status {
                Status::Ok => {
                    row.symbol = '✓';
                    row.status = "OK";
                    row.message = format!("Updated {}", target.name());
                }
                Status::Unavailable(reason) => {
                    row.symbol = '○';
                    row.status = "UNAVAILABLE";
                    row.message = format!("Unavailable {}: {reason}", target.name());
                }
                Status::Skipped(reason) => {
                    row.symbol = '⊘';
                    row.status = "SKIP";
                    row.message = format!("Skipped {}: {reason}", target.name());
                }
                Status::Failed(reason) => {
                    row.symbol = '✗';
                    row.status = "ERR";
                    row.message = format!("Failed {}: {reason}", target.name());
                }
            }
            row.running = false;
            draw_footer(&self.inner, &rows);
        }
    }

    fn is_hidden(&self) -> bool {
        !self.inner.visible
    }

    fn println(&self, line: &str) {
        let rows = lock(&self.inner.rows);
        let _ = emit(false, format!("{line}\n").as_bytes());
        draw_footer(&self.inner, &rows);
    }

    fn write_output(&self, bytes: &[u8], is_stderr: bool) -> io::Result<()> {
        let _rows = lock(&self.inner.rows);
        emit(is_stderr, bytes)
    }
}

fn initialize_footer(inner: &ProgressInner) {
    let count = lock(&inner.rows).len();
    let top = inner.height.saturating_sub(count);
    let setup = if inner.top {
        let first_output_row = count + inner.top_offset + 1;
        format!("\x1b[{first_output_row};{}r\x1b[{first_output_row};1H\x1b[?25l", inner.height)
    } else {
        format!("{}\x1b[{count}A\x1b7\x1b[1;{top}r\x1b8\x1b[?25l", "\n".repeat(count))
    };
    let _ = emit(false, setup.as_bytes());
    let rows = lock(&inner.rows);
    draw_footer(inner, &rows);
}

fn current_row(rows: &[ProgressRow]) -> Option<(usize, &ProgressRow)> {
    rows.iter()
        .enumerate()
        .rev()
        .find(|(_, row)| row.running)
        .or_else(|| rows.iter().enumerate().next_back())
}

fn draw_footer(inner: &ProgressInner, rows: &[ProgressRow]) {
    let frame = if inner.footer {
        rows.iter()
            .enumerate()
            .map(|(index, row)| {
                let line =
                    format_progress_line(index + 1, rows.len(), row, inner.width, inner.colored);
                let terminal_row = progress_terminal_row(
                    index,
                    rows.len(),
                    inner.height,
                    inner.top,
                    inner.top_offset,
                );
                format!("\x1b7\x1b[{terminal_row};1H\x1b[2K{line}\x1b8")
            })
            .collect::<String>()
    } else if inner.visible {
        current_row(rows)
            .map(|(index, row)| {
                let line =
                    format_progress_line(index + 1, rows.len(), row, inner.width, inner.colored);
                format!("\x1b7\x1b[{};1H\x1b[2K{line}\x1b8", inner.top_offset + 1)
            })
            .unwrap_or_default()
    } else {
        String::new()
    };
    if !frame.is_empty() {
        let _ = emit(false, frame.as_bytes());
    }
}

fn progress_terminal_row(
    index: usize,
    count: usize,
    height: usize,
    at_top: bool,
    top_offset: usize,
) -> usize {
    if at_top { top_offset + index + 1 } else { height.saturating_sub(count) + index + 1 }
}

fn format_progress_line(
    index: usize,
    total: usize,
    row: &ProgressRow,
    width: usize,
    colored: bool,
) -> String {
    let left = format!("[{index}/{total}] {} ", row.symbol);
    let left_width = left.chars().count();
    let status = row.status;
    let available = width.saturating_sub(left_width + status.len() + 2);
    let message: String = row.message.chars().take(available).collect();
    let padding = width.saturating_sub(left_width + message.chars().count() + status.len()).max(1);
    let color = match row.status {
        "UPDATING" => "33",
        "OK" => "32",
        "UNAVAILABLE" => "36",
        "SKIP" => "34",
        "ERR" => "31",
        _ => "",
    };
    if colored && !color.is_empty() {
        format!(
            "[{index}/{total}] \x1b[{color}m{}\x1b[0m {message}{}\x1b[{color}m{status}\x1b[0m",
            row.symbol,
            " ".repeat(padding)
        )
    } else {
        format!("{left}{message}{}{status}", " ".repeat(padding))
    }
}

fn teardown_footer(inner: &ProgressInner) {
    let rows = lock(&inner.rows);
    let text = if inner.top {
        format!(
            "\x1b[r\x1b[{};1H\x1b[?25h",
            top_progress_next_row(rows.len(), inner.top_offset, true)
        )
    } else {
        let mut text = String::from("\x1b[r\x1b7");
        for index in 0..rows.len() {
            let terminal_row =
                progress_terminal_row(index, rows.len(), inner.height, false, inner.top_offset);
            text.push_str(&format!("\x1b[{terminal_row};1H\x1b[2K"));
        }
        text.push_str("\x1b8\x1b[?25h\r\n");
        for (index, row) in rows.iter().enumerate() {
            text.push_str(&format_progress_line(
                index + 1,
                rows.len(),
                row,
                inner.width,
                inner.colored,
            ));
            text.push('\n');
        }
        text
    };
    let _ = emit(false, text.as_bytes());
}

fn finish_top_progress(inner: &ProgressInner) {
    let rows = lock(&inner.rows);
    let next_row = top_progress_next_row(rows.len(), inner.top_offset, inner.footer);
    let _ = emit(false, format!("\x1b[{next_row};1H\x1b[?25h").as_bytes());
}

fn top_progress_next_row(count: usize, top_offset: usize, footer: bool) -> usize {
    if footer { count + top_offset + 1 } else { top_offset + 2 }
}

fn write_top_notice(stream: &mut impl Write, notice: &str) -> io::Result<()> {
    write!(stream, "\x1b[1;1H\x1b[2K{notice}\r\n")?;
    stream.flush()
}

#[cfg(not(test))]
fn main() {
    if let Err(error) = run(Cli::parse()) {
        eprintln!("mnn: {error}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<(), String> {
    if let Some(mode) = cli.print_completion {
        print_completion(mode);
        return Ok(());
    }

    let action_count =
        [cli.install, cli.reinstall, cli.update, cli.uninstall, cli.purge, cli.gen_config]
            .into_iter()
            .filter(|value| *value)
            .count();

    let mut settings = load_settings()?;
    if !cli.targets.is_empty() {
        settings.targets = cli.targets.clone();
    }
    if cli.silent || cli.verbose {
        settings.silent = cli.silent;
        settings.verbose = cli.verbose;
    }
    if cli.parallel || cli.no_parallel {
        settings.parallel = cli.parallel;
    }
    if cli.disable_log {
        settings.disable_log = true;
    }
    if cli.dry_run {
        settings.silent = false;
    }

    if action_count > 0 {
        if !cli.targets.is_empty() {
            return Err("maintenance options cannot be combined with targets".to_owned());
        }
        if cli.gen_config {
            generate_config()?;
            return Ok(());
        }
        let action = if cli.install {
            InstallAction::Install
        } else if cli.reinstall {
            InstallAction::Reinstall
        } else if cli.update {
            InstallAction::Update
        } else if cli.uninstall {
            InstallAction::Uninstall
        } else {
            InstallAction::Purge
        };
        return maintain(action, cli.install_completions);
    }
    if let Some(mode) = cli.install_completions {
        if !cli.targets.is_empty() {
            return Err("--install-completions cannot be combined with targets".to_owned());
        }
        install_completions(mode)?;
        return Ok(());
    }

    #[cfg(unix)]
    ensure_not_root(effective_uid())?;

    if settings.silent && settings.verbose {
        return Err("--silent and --verbose cannot be used together".to_owned());
    }
    let targets = expand_targets(&settings.targets);
    if targets.is_empty() {
        return Err("no supported package managers detected".to_owned());
    }
    let log =
        if settings.disable_log { None } else { Some(Arc::new(Mutex::new(open_log_file()?))) };
    if let Some(log) = &log {
        let names = targets.iter().map(|target| target.name()).collect::<Vec<_>>().join(",");
        let mode = if settings.parallel { "parallel" } else { "sequential" };
        write_log(
            log,
            &format!("mnn started: targets={names} mode={mode} dry_run={}", cli.dry_run),
        )?;
    }
    let _run_lock = acquire_run_lock()?;
    let show_progress =
        progress_wanted(cli.dry_run, io::stdout().is_terminal(), std::env::var_os("TERM"));
    let reserve_sudo_line =
        show_progress && settings.silent && !cli.dry_run && targets_need_sudo(&targets, &settings);
    let _sudo_keepalive =
        prepare_sudo_for_targets(&targets, &settings, cli.dry_run, reserve_sudo_line)?;
    if cli.dry_run {
        println!("Dry run; no package managers will be changed.");
    }
    let top_offset = usize::from(reserve_sudo_line);
    let mut progress = ProgressManager::new(show_progress, settings.silent, top_offset, &targets);
    let progress_bars: Vec<_> = (0..targets.len()).map(|index| progress.bar(index)).collect();
    let system_lane = Arc::new(Mutex::new(()));
    let results = execute_targets(&targets, settings.parallel, &|index, target| {
        run_target(target, &settings, cli.dry_run, &system_lane, log.clone(), &progress_bars[index])
    });

    progress.finish();
    let [succeeded, unavailable, skipped, failed] =
        report(results, settings.silent, show_progress, log.as_ref())?;
    println!(
        "{succeeded} succeeded, {unavailable} unavailable, {skipped} skipped, {failed} failed"
    );
    if failed > 0 { Err("one or more package-manager targets failed".to_owned()) } else { Ok(()) }
}

fn report(
    results: Vec<TargetResult>,
    silent: bool,
    show_progress: bool,
    log: Option<&Arc<Mutex<File>>>,
) -> Result<[usize; 4], String> {
    let mut counts = [0; 4];
    for result in results {
        for line in result.output.iter().filter(|line| !silent && !line.is_empty()) {
            println!("{line}");
        }
        let name = result.target.name();
        let (slot, symbol, detail, label) = match result.status {
            Status::Ok => (0, '✓', "OK".to_owned(), "OK".to_owned()),
            Status::Unavailable(reason) => {
                (1, '○', reason.clone(), format!("UNAVAILABLE ({reason})"))
            }
            Status::Skipped(reason) => (2, '⊘', reason.clone(), format!("SKIP ({reason})")),
            Status::Failed(reason) => (3, '✗', reason.clone(), format!("ERR ({reason})")),
        };
        if let Some(log) = log {
            write_log(log, &format!("target {name}: {label}"))?;
        }
        counts[slot] += 1;
        if !show_progress {
            let line = format!("{symbol} {name}: {detail}");
            if slot == 3 {
                eprintln!("{line}");
            } else {
                println!("{line}");
            }
        }
    }
    Ok(counts)
}

fn progress_wanted(dry_run: bool, terminal: bool, term: Option<OsString>) -> bool {
    !dry_run && terminal && term.is_some_and(|term| term != "dumb")
}

fn execute_targets(
    targets: &[Target],
    parallel: bool,
    execute: &(dyn Fn(usize, Target) -> TargetResult + Sync),
) -> Vec<TargetResult> {
    if parallel && targets.len() > 1 {
        std::thread::scope(|scope| {
            let handles: Vec<_> = targets
                .iter()
                .copied()
                .enumerate()
                .map(|(index, target)| (target, scope.spawn(move || execute(index, target))))
                .collect();
            handles
                .into_iter()
                .map(|(target, handle)| {
                    handle.join().unwrap_or_else(|_| TargetResult {
                        target,
                        status: Status::Failed("worker thread panicked".to_owned()),
                        output: Vec::new(),
                    })
                })
                .collect()
        })
    } else {
        targets.iter().copied().enumerate().map(|(index, target)| execute(index, target)).collect()
    }
}

#[cfg(unix)]
fn ensure_not_root(uid: u32) -> Result<(), String> {
    if uid == 0 { Err("do not run as root; run as a regular user".to_owned()) } else { Ok(()) }
}

#[cfg(unix)]
fn effective_uid() -> u32 {
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: geteuid has no preconditions and returns the effective user id.
    unsafe { geteuid() }
}

fn load_settings() -> Result<Settings, String> {
    let mut settings = Settings {
        targets: vec![Target::Auto],
        silent: false,
        verbose: false,
        parallel: false,
        disable_log: false,
    };
    let path = config_path()?;
    validate_user_file(&path, "config")?;
    let content = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(settings),
        Err(error) => return Err(format!("cannot read config {}: {error}", path.display())),
    };
    for (line_number, raw) in content.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or_default().trim();
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| format!("invalid config line {}: {raw}", line_number + 1))?;
        let key = key.trim();
        let value = value.trim();
        match key {
            "targets" => {
                settings.targets = value
                    .split([',', ' ', '\t'])
                    .filter(|item| !item.is_empty())
                    .map(|name| {
                        Target::from_str(name, true)
                            .map_err(|_| format!("unknown target in config: {name}"))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                if settings.targets.is_empty() {
                    return Err("targets config cannot be empty".to_owned());
                }
            }
            "verbose" => settings.verbose = parse_bool(key, value)?,
            "silent" => settings.silent = parse_bool(key, value)?,
            "parallel" => settings.parallel = parse_bool(key, value)?,
            "disable-log" => settings.disable_log = parse_bool(key, value)?,
            _ => return Err(format!("unknown config setting: {key}")),
        }
    }
    Ok(settings)
}

fn parse_bool(key: &str, value: &str) -> Result<bool, String> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "1" => Ok(true),
        "false" | "no" | "0" => Ok(false),
        _ => Err(format!("invalid config setting: {key}={value}")),
    }
}

fn expand_targets(requested: &[Target]) -> Vec<Target> {
    let mut selected = BTreeSet::new();
    for target in requested {
        match target {
            Target::Auto => selected.extend(detect_targets()),
            Target::All => selected.extend(Target::all()),
            Target::Debian => {
                selected.insert(Target::Apt);
            }
            other => {
                selected.insert(*other);
            }
        }
    }
    let mut selected: Vec<_> = selected.into_iter().collect();
    selected.sort_by_key(|target| target.name());
    selected
}

fn detect_targets() -> Vec<Target> {
    let (os_id, os_like) = read_os_release();
    detect_targets_for(&os_id, &os_like)
}

fn detect_targets_for(os_id: &str, os_like: &str) -> Vec<Target> {
    let is = |names: &[&str]| names.iter().any(|name| os_id == *name || os_like.contains(name));
    let has = |command: &str| find_program(OsStr::new(command)).is_some();
    let mut targets = Vec::new();
    if has("apt") {
        targets.push(Target::Apt);
    }
    if has("rpm-ostree") {
        targets.push(Target::RpmOstree);
    }
    if has("transactional-update") {
        targets.push(Target::TransactionalUpdate);
    }
    if has("dnf")
        && !has("rpm-ostree")
        && (is(&["fedora", "rhel", "centos", "rocky", "almalinux"]) || os_id.is_empty())
    {
        targets.push(Target::Dnf);
    }
    if has("yum")
        && !has("dnf")
        && (is(&["rhel", "centos", "fedora", "rocky", "almalinux"]) || os_id.is_empty())
    {
        targets.push(Target::Yum);
    }
    if has("zypper")
        && !has("transactional-update")
        && (is(&["suse", "opensuse"]) || os_id.is_empty())
    {
        targets.push(Target::Zypper);
    }
    if has("pacman") && (is(&["arch", "manjaro"]) || os_id.is_empty()) {
        targets.push(Target::Pacman);
        if ["yay", "paru", "pikaur", "trizen"].iter().any(|cmd| has(cmd)) {
            targets.push(Target::Aur);
        }
    }
    if has("xbps-install") && (is(&["void"]) || os_id.is_empty()) {
        targets.push(Target::Xbps);
    }
    if has("apk") && (is(&["alpine"]) || os_id.is_empty()) {
        targets.push(Target::Apk);
    }
    if has("emerge") && (is(&["gentoo"]) || os_id.is_empty()) {
        targets.push(Target::Emerge);
    }
    if has("eopkg") && (is(&["solus"]) || os_id.is_empty()) {
        targets.push(Target::Eopkg);
    }
    #[cfg(target_os = "freebsd")]
    if has("pkg") {
        targets.push(Target::Pkg);
    }
    if has("brew") {
        targets.push(Target::Brew);
    }
    for (cmd, target) in [
        ("snap", Target::Snap),
        ("flatpak", Target::Flatpak),
        ("soar", Target::Soar),
        ("determinate-nixd", Target::Determinate),
        ("uv", Target::Uv),
    ] {
        if has(cmd) {
            targets.push(target);
        }
    }
    if script_installed("bun", "BUN_INSTALL", ".bun") {
        targets.push(Target::Bun);
    }
    if script_installed("pnpm", "PNPM_HOME", ".local/share/pnpm") {
        targets.push(Target::Pnpm);
    }
    if pip_available() {
        targets.push(Target::Pip);
    }
    #[cfg(windows)]
    {
        if has("winget") {
            targets.push(Target::Winget);
        }
        if has("choco") {
            targets.push(Target::Choco);
        }
    }
    targets
}

fn pip_available() -> bool {
    pip_interpreter_with(&find_program, &pip_installation_available).is_some()
}

fn pip_interpreter_with(
    find: &dyn Fn(&OsStr) -> Option<PathBuf>,
    is_installed: &dyn Fn(&str, &Path) -> bool,
) -> Option<(String, PathBuf)> {
    ["pip", "pip3", "python", "python3", "py"].iter().find_map(|candidate| {
        let executable = find(OsStr::new(candidate))?;
        is_installed(candidate, &executable).then(|| ((*candidate).to_owned(), executable))
    })
}

fn pip_installation_available(program: &str, executable: &Path) -> bool {
    let mut command = Command::new(executable);
    match program {
        "python" | "python3" => {
            command.args(["-m", "pip", "--version"]);
        }
        "py" => {
            command.args(["-3", "-m", "pip", "--version"]);
        }
        _ => {
            command.arg("--version");
        }
    }
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn read_os_release() -> (String, String) {
    parse_os_release(&fs::read_to_string("/etc/os-release").unwrap_or_default())
}

fn parse_os_release(data: &str) -> (String, String) {
    let mut id = String::new();
    let mut like = String::new();
    for line in data.lines() {
        if let Some(value) = line.strip_prefix("ID=") {
            id = value.trim_matches('"').to_ascii_lowercase();
        } else if let Some(value) = line.strip_prefix("ID_LIKE=") {
            like = value.trim_matches('"').to_ascii_lowercase();
        }
    }
    (id, like)
}

fn is_termux() -> bool {
    std::env::var_os("TERMUX_VERSION").is_some() || Path::new("/data/data/com.termux").is_dir()
}

fn script_installed(command: &str, variable: &str, home_suffix: &str) -> bool {
    let Some(path) = find_program(OsStr::new(command)) else {
        return false;
    };
    let base = std::env::var_os(variable)
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|home| home.join(home_suffix)));
    base.is_some_and(|base| path.starts_with(base))
}

fn run_target(
    target: Target,
    settings: &Settings,
    dry_run: bool,
    system_lane: &Mutex<()>,
    log: SharedLog,
    progress: &ProgressBar,
) -> TargetResult {
    let mut result = TargetResult { target, status: Status::Ok, output: Vec::new() };
    progress.set_message(&format!("Updating {}...", target.name()));
    if target == Target::Bun && !script_installed("bun", "BUN_INSTALL", ".bun") {
        result.status =
            Status::Unavailable("bun was not installed by its install script".to_owned());
        finish_target_progress(progress, target, &result.status);
        return result;
    }
    if target == Target::Pnpm && !script_installed("pnpm", "PNPM_HOME", ".local/share/pnpm") {
        result.status =
            Status::Unavailable("pnpm was not installed by its install script".to_owned());
        finish_target_progress(progress, target, &result.status);
        return result;
    }
    if target == Target::Aur && target_steps(target, settings.verbose).is_empty() {
        result.status = Status::Unavailable("no supported AUR helper found".to_owned());
        finish_target_progress(progress, target, &result.status);
        return result;
    }
    let serial_system_lane = target.needs_sudo() || target == Target::Aur;
    if serial_system_lane {
        let _lane = lock(system_lane);
        let steps = target_steps(target, settings.verbose);
        if target.needs_sudo() && steps.iter().any(|step| !trusted_program_exists(&step.program)) {
            result.status = Status::Unavailable(
                "required command is not a trusted system executable".to_owned(),
            );
            finish_target_progress(progress, target, &result.status);
            return result;
        }
        run_target_inner(target, settings, dry_run, &mut result, log.as_ref(), progress);
    } else {
        run_target_inner(target, settings, dry_run, &mut result, log.as_ref(), progress);
    }
    finish_target_progress(progress, target, &result.status);
    result
}

fn finish_target_progress(progress: &ProgressBar, target: Target, status: &Status) {
    progress.finish(target, status);
}

fn run_target_inner(
    target: Target,
    settings: &Settings,
    dry_run: bool,
    result: &mut TargetResult,
    log: Option<&Arc<Mutex<File>>>,
    progress: &ProgressBar,
) {
    if target == Target::Bun && !script_installed("bun", "BUN_INSTALL", ".bun") {
        result.status =
            Status::Unavailable("bun was not installed by its install script".to_owned());
        return;
    }
    if target == Target::Pnpm && !script_installed("pnpm", "PNPM_HOME", ".local/share/pnpm") {
        result.status =
            Status::Unavailable("pnpm was not installed by its install script".to_owned());
        return;
    }
    if target == Target::Pip {
        result.status = run_pip(settings, dry_run, &mut result.output, log, progress);
        return;
    }
    let steps = target_steps(target, settings.verbose);
    if steps.is_empty() {
        result.status = Status::Skipped("no commands configured".to_owned());
        return;
    }
    for step in steps {
        progress.set_message(step.description.trim_end_matches('.'));
        if step.system && target.needs_sudo() && !trusted_program_exists(&step.program) {
            result.status =
                Status::Unavailable(format!("{} is not a trusted system executable", step.program));
            return;
        }
        if !step.system && find_program(OsStr::new(&step.program)).is_none() {
            result.status = Status::Unavailable(format!("{} not found", step.program));
            return;
        }
        match execute_step(&step, dry_run, settings, log, progress) {
            Ok(output) => {
                if !output.trim().is_empty() {
                    result.output.push(output);
                }
            }
            Err(error) => {
                result.status = Status::Failed(error);
                return;
            }
        }
    }
}

fn target_steps(target: Target, verbose: bool) -> Vec<Step> {
    let sys = target.needs_sudo();
    let mut steps = match target {
        Target::Apk => vec![
            Step::new("Updating apk indexes...", "apk", &["update"], sys),
            Step::new("Upgrading apk packages...", "apk", &["upgrade"], sys),
        ],
        Target::Apt | Target::Debian => {
            let quiet: &[&str] = if verbose { &[] } else { &["-qq"] };
            let mut update = vec!["update"];
            update.splice(0..0, quiet.iter().copied());
            vec![
                Step::new("Updating apt package index...", "apt-get", &update, sys),
                Step::new(
                    "Upgrading apt packages...",
                    "apt-get",
                    &[
                        "-y",
                        "-o",
                        "Dpkg::Use-Pty=0",
                        "-o",
                        "Dpkg::Options::=--force-confdef",
                        "-o",
                        "Dpkg::Options::=--force-confold",
                        "-o",
                        "APT::Get::Always-Include-Phased-Updates=true",
                        "full-upgrade",
                    ],
                    sys,
                )
                .with_env(&[
                    ("DEBIAN_FRONTEND", "noninteractive"),
                    ("NEEDRESTART_MODE", "a"),
                    ("APT_LISTCHANGES_FRONTEND", "none"),
                ]),
                Step::new(
                    "Removing obsolete apt packages...",
                    "apt-get",
                    &["-y", "autoremove", "--purge"],
                    sys,
                )
                .with_env(&[("DEBIAN_FRONTEND", "noninteractive"), ("NEEDRESTART_MODE", "a")]),
            ]
        }
        Target::Aur => {
            let Some(helper) = ["yay", "paru", "pikaur", "trizen"]
                .iter()
                .find(|helper| find_program(OsStr::new(helper)).is_some())
            else {
                return Vec::new();
            };
            let mut args = vec!["-Syu", "--noconfirm"];
            match *helper {
                "yay" => args.extend([
                    "--answerclean",
                    "None",
                    "--answerdiff",
                    "None",
                    "--answeredit",
                    "None",
                ]),
                "paru" => args.push("--skipreview"),
                "pikaur" => args.extend(["--noedit", "--nodiff"]),
                _ => {}
            }
            vec![Step::new("Upgrading AUR packages...", helper, &args, false)]
        }
        Target::Brew => vec![
            Step::new("Updating brew metadata...", "brew", &["update"], false),
            Step::new("Upgrading brew packages...", "brew", &["upgrade"], false).with_env(&[
                ("HOMEBREW_NO_ASK", "1"),
                ("HOMEBREW_NO_AUTO_UPDATE", "1"),
                ("HOMEBREW_NO_ENV_HINTS", "1"),
            ]),
            Step::new("Cleaning brew cache...", "brew", &["cleanup"], false).with_env(&[
                ("HOMEBREW_NO_ASK", "1"),
                ("HOMEBREW_NO_AUTO_UPDATE", "1"),
                ("HOMEBREW_NO_ENV_HINTS", "1"),
            ]),
        ],
        Target::Bun => vec![Step::new("Upgrading bun...", "bun", &["upgrade"], false)],
        Target::Choco => vec![Step::new(
            "Upgrading Chocolatey packages...",
            "choco",
            &["upgrade", "all", "--yes", "--no-progress"],
            false,
        )],
        Target::Determinate => vec![Step::new(
            "Upgrading determinate packages...",
            "determinate-nixd",
            &["upgrade"],
            sys,
        )],
        Target::Dnf => vec![
            Step::new("Upgrading dnf packages...", "dnf", &["-y", "upgrade", "--refresh"], sys),
            Step::new("Removing obsolete dnf packages...", "dnf", &["-y", "autoremove"], sys),
        ],
        Target::Emerge => vec![
            Step::new("Synchronizing emerge repositories...", "emerge", &["--sync"], sys),
            Step::new(
                "Updating emerge world set...",
                "emerge",
                &["--ask=n", "--update", "--deep", "--newuse", "@world"],
                sys,
            ),
        ],
        Target::Eopkg => {
            vec![Step::new("Upgrading eopkg packages...", "eopkg", &["-y", "upgrade"], sys)]
        }
        Target::Flatpak => vec![Step::new(
            "Updating flatpak packages...",
            "flatpak",
            &["update", "--assumeyes", "--noninteractive"],
            false,
        )],
        Target::Pacman => {
            vec![Step::new("Upgrading pacman packages...", "pacman", &["-Syu", "--noconfirm"], sys)]
        }
        Target::Pkg if is_termux() => vec![
            Step::new("Updating pkg repositories...", "pkg", &["update"], false),
            Step::new(
                "Upgrading pkg packages...",
                "pkg",
                &[
                    "upgrade",
                    "-y",
                    "-o",
                    "Dpkg::Options::=--force-confdef",
                    "-o",
                    "Dpkg::Options::=--force-confold",
                ],
                false,
            )
            .with_env(&[("DEBIAN_FRONTEND", "noninteractive")]),
            Step::new(
                "Removing orphaned pkg packages...",
                "apt",
                &["-y", "autoremove", "--purge"],
                false,
            )
            .with_env(&[("DEBIAN_FRONTEND", "noninteractive")]),
        ],
        Target::Pkg => vec![
            Step::new("Updating pkg repositories...", "pkg", &["update"], sys)
                .with_env(&[("ASSUME_ALWAYS_YES", "yes")]),
            Step::new("Upgrading pkg packages...", "pkg", &["upgrade", "-y"], sys)
                .with_env(&[("ASSUME_ALWAYS_YES", "yes")]),
            Step::new("Removing orphaned pkg packages...", "pkg", &["autoremove", "-y"], sys)
                .with_env(&[("ASSUME_ALWAYS_YES", "yes")]),
        ],
        Target::Pip => Vec::new(),
        Target::Pnpm => vec![Step::new("Upgrading pnpm...", "pnpm", &["self-update"], false)],
        Target::RpmOstree => {
            vec![Step::new("Upgrading rpm-ostree deployment...", "rpm-ostree", &["upgrade"], sys)]
        }
        Target::Snap => vec![Step::new("Refreshing snap packages...", "snap", &["refresh"], sys)],
        Target::Soar => vec![
            Step::new("Syncing soar index...", "soar", &["sync"], false),
            Step::new("Updating soar packages...", "soar", &["update"], false),
        ],
        Target::TransactionalUpdate => vec![Step::new(
            "Running transactional update...",
            "transactional-update",
            &["-n", "dup"],
            sys,
        )],
        Target::Uv => {
            vec![Step::new("Upgrading uv tools...", "uv", &["tool", "upgrade", "--all"], false)]
        }
        Target::Winget => vec![Step::new(
            "Upgrading Windows packages with winget...",
            "winget",
            &[
                "upgrade",
                "--all",
                "--silent",
                "--accept-source-agreements",
                "--accept-package-agreements",
                "--disable-interactivity",
            ],
            false,
        )],
        Target::Xbps => vec![
            Step::new("Synchronizing xbps repositories...", "xbps-install", &["-S"], sys),
            Step::new("Updating xbps itself...", "xbps-install", &["-yu", "xbps"], sys),
            Step::new("Updating xbps packages...", "xbps-install", &["-yu"], sys),
        ],
        Target::Yum => vec![
            Step::new("Updating yum packages...", "yum", &["-y", "update"], sys),
            Step::new("Removing obsolete yum packages...", "yum", &["-y", "autoremove"], sys),
        ],
        Target::Zypper => {
            let action = zypper_action(&read_os_release().0);
            vec![
                Step::new(
                    "Refreshing zypper repositories...",
                    "zypper",
                    &["--non-interactive", "refresh"],
                    sys,
                ),
                Step::new(
                    "Updating zypper packages...",
                    "zypper",
                    &["--non-interactive", action, "--auto-agree-with-licenses"],
                    sys,
                ),
            ]
        }
        Target::Auto | Target::All => Vec::new(),
    };
    for step in &mut steps {
        if target == Target::Bun && !script_installed("bun", "BUN_INSTALL", ".bun") {
            step.description = "Skipping bun (not installed by its install script)...".to_owned();
        }
        if target == Target::Pnpm && !script_installed("pnpm", "PNPM_HOME", ".local/share/pnpm") {
            step.description = "Skipping pnpm (not installed by its install script)...".to_owned();
        }
    }
    steps
}

fn zypper_action(os_id: &str) -> &'static str {
    if os_id.contains("tumbleweed") { "dup" } else { "update" }
}

fn run_pip(
    settings: &Settings,
    dry_run: bool,
    output: &mut Vec<String>,
    log: Option<&Arc<Mutex<File>>>,
    progress: &ProgressBar,
) -> Status {
    let interpreter = pip_interpreter_with(&find_program, &pip_installation_available);
    let Some((program, executable)) = interpreter else {
        return Status::Unavailable("python or pip not found".to_owned());
    };
    let mut prefix = Vec::<OsString>::new();
    if program == "python" || program == "python3" || program == "py" {
        if program == "py" {
            prefix.push(OsString::from("-3"));
        }
        prefix.extend(["-m", "pip"].into_iter().map(OsString::from));
    }
    let query_args = prefix
        .iter()
        .cloned()
        .chain(["list", "--outdated", "--format=json"].into_iter().map(OsString::from))
        .collect::<Vec<_>>();
    let display = format_command(&program, &query_args);
    progress.set_message("Checking outdated Python packages...");
    if dry_run {
        output.push(format!("Would inspect outdated Python packages with: {display}"));
        let update = prefix
            .iter()
            .cloned()
            .chain(["install", "--upgrade", "<package>"].map(OsString::from))
            .collect::<Vec<_>>();
        output.push(format!("For each outdated package: {}", format_command(&program, &update)));
        return Status::Ok;
    }
    let mut command = Command::new(&executable);
    command.args(&query_args);
    let listing = match command.stdin(std::process::Stdio::null()).output() {
        Ok(value) if value.status.success() => value,
        Ok(value) => {
            let detail = output_text(&value);
            if let Some(log) = log
                && let Err(error) = write_log(log, &format!("pip outdated check:\n{detail}"))
            {
                return Status::Failed(error);
            }
            if detail.contains("No module named pip") {
                return Status::Unavailable(
                    "pip is not installed for the selected Python".to_owned(),
                );
            }
            return Status::Failed(if settings.silent || detail.trim().is_empty() {
                format!("pip outdated check exited with {}", value.status)
            } else {
                format!("pip outdated check exited with {}: {}", value.status, detail.trim())
            });
        }
        Err(error) => return Status::Failed(format!("could not run pip: {error}")),
    };
    let packages: Vec<Value> = match serde_json::from_slice(&listing.stdout) {
        Ok(packages) => packages,
        Err(error) => return Status::Failed(format!("pip returned invalid package data: {error}")),
    };
    for package in packages {
        let Some(name) = package.get("name").and_then(Value::as_str) else {
            continue;
        };
        let mut args = prefix.clone();
        args.extend(["install", "--upgrade"].into_iter().map(OsString::from));
        args.push(OsString::from(name));
        let step = Step {
            description: format!("Upgrading Python package {name}..."),
            program: program.clone(),
            args,
            env: Vec::new(),
            system: false,
        };
        progress.set_message(&format!("Upgrading Python package {name}..."));
        if let Err(error) = execute_step(&step, false, settings, log, progress) {
            return Status::Failed(error);
        }
    }
    Status::Ok
}

fn execute_step(
    step: &Step,
    dry_run: bool,
    settings: &Settings,
    log: Option<&Arc<Mutex<File>>>,
    progress: &ProgressBar,
) -> Result<String, String> {
    execute_step_attempt(step, dry_run, settings, log, progress, true)
}

fn execute_step_attempt(
    step: &Step,
    dry_run: bool,
    settings: &Settings,
    log: Option<&Arc<Mutex<File>>>,
    progress: &ProgressBar,
    retry_zypper: bool,
) -> Result<String, String> {
    let sudo_required = step_needs_sudo(step);
    let manager = if sudo_required {
        trusted_program(&step.program)
    } else {
        find_program(OsStr::new(&step.program))
    }
    .ok_or_else(|| format!("{} not found or is not trusted", step.program))?;
    let mut command = if sudo_required {
        let sudo = find_trusted_sudo().ok_or_else(missing("no trusted sudo executable found"))?;
        let mut command = Command::new(sudo);
        command.args(["-n", "--"]);
        if step.env.is_empty() {
            command.arg(manager);
        } else {
            let env =
                trusted_program("env").ok_or_else(missing("no trusted env executable found"))?;
            command.arg(env);
            for (key, value) in &step.env {
                command.arg(format!("{}={}", key.to_string_lossy(), value.to_string_lossy()));
            }
            command.arg(manager);
        }
        command
    } else {
        Command::new(manager)
    };
    command.args(&step.args);
    if !sudo_required {
        for (key, value) in &step.env {
            command.env(key, value);
        }
    }
    if dry_run {
        let mut parts = Vec::new();
        if step.system && step_needs_sudo(step) {
            parts.push("sudo".to_owned());
        }
        parts.push(step.program.clone());
        let args: Vec<String> =
            step.args.iter().map(|arg| arg.to_string_lossy().into_owned()).collect();
        let vars: Vec<String> = step
            .env
            .iter()
            .map(|(key, value)| format!("{}={}", key.to_string_lossy(), value.to_string_lossy()))
            .collect();
        return Ok(format!(
            "{} {}",
            step.description,
            format_command_with_env(&parts, &vars, &args)
        ));
    }
    let shown = format_command(&step.program, &step.args);
    if settings.verbose {
        eprintln!("> {shown}");
    }
    if let Some(log) = log {
        write_log(log, &format!("----- output: {shown} -----"))?;
    }
    let filter_brew_download_progress = !settings.verbose
        && step.program == "brew"
        && step.args.first().is_some_and(|arg| arg == "update");
    let (status, text) =
        stream_command(command, settings, log, progress, filter_brew_download_progress)
            .map_err(describe(&step.description))?;
    if status.success() {
        Ok(String::new())
    } else {
        let is_zypper_update =
            step.program == "zypper" && step.args.iter().any(|arg| arg == "update" || arg == "dup");
        match (is_zypper_update, status.code()) {
            (true, Some(102)) => {
                let message = "zypper finished; a reboot is required to complete the update.";
                if let Some(log) = log {
                    write_log(log, message)?;
                }
                if !settings.silent {
                    if progress.is_hidden() {
                        println!("{message}");
                    } else {
                        progress.println(message);
                    }
                }
                Ok(String::new())
            }
            (true, Some(103)) if retry_zypper => {
                if let Some(log) = log {
                    write_log(log, "zypper updated itself; retrying the update.")?;
                }
                execute_step_attempt(step, false, settings, log, progress, false)
            }
            _ => {
                let detail = if settings.silent || text.trim().is_empty() {
                    String::new()
                } else {
                    format!(": {}", text.trim())
                };
                Err(format!("{} exited with {}{detail}", step.description, status))
            }
        }
    }
}

fn stream_command(
    mut command: Command,
    settings: &Settings,
    log: Option<&Arc<Mutex<File>>>,
    progress: &ProgressBar,
    filter_brew_download_progress: bool,
) -> io::Result<(std::process::ExitStatus, String)> {
    use std::process::Stdio;

    command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let stdout = child.stdout.take().expect("piped stdout is available");
    let stderr = child.stderr.take().expect("piped stderr is available");
    let stdout_thread = spawn_output_reader(
        Box::new(stdout),
        false,
        settings.silent,
        log.cloned(),
        progress.clone(),
        filter_brew_download_progress,
    );
    let stderr_thread = spawn_output_reader(
        Box::new(stderr),
        true,
        settings.silent,
        log.cloned(),
        progress.clone(),
        filter_brew_download_progress,
    );
    let status = child.wait()?;
    let stdout_text = join_reader(stdout_thread)?;
    let stderr_text = join_reader(stderr_thread)?;
    let text = match (stdout_text.is_empty(), stderr_text.is_empty()) {
        (true, _) => stderr_text,
        (_, true) => stdout_text,
        (false, false) => format!("{stdout_text}\n{stderr_text}"),
    };
    Ok((status, text))
}

fn join_reader(handle: JoinHandle<io::Result<String>>) -> io::Result<String> {
    handle.join().map_err(|_| io::Error::other("output reader thread panicked"))?
}

fn spawn_output_reader(
    reader: Box<dyn Read + Send>,
    is_stderr: bool,
    silent: bool,
    log: SharedLog,
    progress: ProgressBar,
    filter_brew_download_progress: bool,
) -> JoinHandle<io::Result<String>> {
    std::thread::spawn(move || {
        let mut output = String::new();
        let mut chunk = [0_u8; 4096];
        let mut log_line = Vec::new();
        let mut display_line = Vec::new();
        let mut previous_was_cr = false;
        let mut reader = reader;
        loop {
            let count = reader.read(&mut chunk)?;
            if count == 0 {
                break;
            }
            let bytes = &chunk[..count];
            output.push_str(&String::from_utf8_lossy(bytes));
            if !silent && !filter_brew_download_progress {
                progress.write_output(bytes, is_stderr)?;
            }
            for byte in bytes {
                if matches!(*byte, b'\r' | b'\n') {
                    if filter_brew_download_progress {
                        if !silent && !is_brew_download_progress_line(&display_line) {
                            progress.write_output(&display_line, is_stderr)?;
                            progress.write_output(&[*byte], is_stderr)?;
                        }
                        display_line.clear();
                    }
                    if *byte == b'\n' && previous_was_cr {
                        previous_was_cr = false;
                        continue;
                    }
                    if !log_line.is_empty() {
                        append_logged_output_line(&log, &log_line)?;
                        log_line.clear();
                    }
                    previous_was_cr = *byte == b'\r';
                } else {
                    if filter_brew_download_progress {
                        display_line.push(*byte);
                    }
                    previous_was_cr = false;
                    log_line.push(*byte);
                }
            }
        }
        if filter_brew_download_progress
            && !silent
            && !display_line.is_empty()
            && !is_brew_download_progress_line(&display_line)
        {
            progress.write_output(&display_line, is_stderr)?;
        }
        if !log_line.is_empty() {
            append_logged_output_line(&log, &log_line)?;
        }
        Ok(output)
    })
}

fn is_brew_download_progress_line(line: &[u8]) -> bool {
    line.strip_prefix(b"Downloading [")
        .and_then(|line| line.strip_suffix(b"]"))
        .is_some_and(|count| !count.is_empty() && count.iter().all(u8::is_ascii_digit))
}

fn append_logged_output_line(log: &SharedLog, bytes: &[u8]) -> io::Result<()> {
    if let Some(log) = log {
        append_log(log, &String::from_utf8_lossy(bytes)).map_err(io::Error::other)?;
    }
    Ok(())
}

fn step_needs_sudo(step: &Step) -> bool {
    step.system
}

fn prepare_sudo_for_targets(
    targets: &[Target],
    settings: &Settings,
    dry_run: bool,
    reserve_top_line: bool,
) -> Result<Option<SudoKeepalive>, String> {
    if dry_run || !targets_need_sudo(targets, settings) {
        return Ok(None);
    }
    let sudo = find_trusted_sudo().ok_or_else(|| {
        "sudo is required for selected targets but no trusted sudo binary was found".to_owned()
    })?;
    let notice = if settings.verbose {
        "Refreshing sudo credentials..."
    } else {
        "mnn: refreshing sudo credentials before starting progress..."
    };
    if reserve_top_line {
        write_top_notice(&mut Console, notice)
            .map_err(describe("could not display sudo prompt"))?;
    } else if settings.verbose {
        eprintln!("Refreshing sudo credentials...");
    } else {
        eprintln!("{notice}");
    }
    io::stderr().flush().map_err(describe("could not display sudo prompt"))?;
    let status =
        Command::new(&sudo).arg("-v").status().map_err(describe("could not start sudo"))?;
    if status.success() {
        Ok(Some(SudoKeepalive::start(sudo)))
    } else {
        Err("sudo authentication failed".to_owned())
    }
}

fn targets_need_sudo(targets: &[Target], settings: &Settings) -> bool {
    targets.iter().any(|target| {
        target.needs_sudo()
            || (*target == Target::Aur && !target_steps(*target, settings.verbose).is_empty())
    })
}

fn output_text(output: &Output) -> String {
    let mut result = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.is_empty() {
        if !result.is_empty() && !result.ends_with('\n') {
            result.push('\n');
        }
        result.push_str(&stderr);
    }
    result
}

fn write_log(log: &Arc<Mutex<File>>, text: &str) -> Result<(), String> {
    append_log(log, text).map_err(describe("could not write log"))
}

fn append_log(log: &Arc<Mutex<File>>, text: &str) -> io::Result<()> {
    let mut file = lock(log);
    writeln!(file, "{text}")
}

fn find_program(name: &OsStr) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(name);
        if candidate.is_file() && is_executable(&candidate) {
            return Some(candidate);
        }
        #[cfg(windows)]
        for extension in ["exe", "cmd", "bat"] {
            let candidate = directory.join(name).with_extension(extension);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path)
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

fn trusted_program_exists(name: &str) -> bool {
    trusted_program(name).is_some()
}

fn trusted_directories() -> Vec<PathBuf> {
    #[cfg(test)]
    if let Some(directories) = lock(&TRUSTED_OVERRIDE).clone() {
        return directories;
    }
    ["/usr/local/sbin", "/usr/local/bin", "/usr/sbin", "/usr/bin", "/sbin", "/bin"]
        .map(PathBuf::from)
        .to_vec()
}

#[cfg(test)]
static TRUSTED_OVERRIDE: Mutex<Option<Vec<PathBuf>>> = Mutex::new(None);

#[cfg(unix)]
fn trusted_owner() -> u32 {
    if cfg!(test) { effective_uid() } else { 0 }
}

fn trusted_program(name: &str) -> Option<PathBuf> {
    #[cfg(unix)]
    {
        trusted_directories()
            .into_iter()
            .map(|directory| directory.join(name))
            .find(|candidate| trusted_path(candidate))
    }
    #[cfg(not(unix))]
    {
        let _ = name;
        None
    }
}

#[cfg(unix)]
fn trusted_path(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(resolved) = fs::canonicalize(path) else {
        return false;
    };
    let parent = resolved.parent().unwrap_or(Path::new("/"));
    fs::symlink_metadata(&resolved).ok().zip(fs::metadata(parent).ok()).is_some_and(
        |(metadata, parent)| {
            metadata.is_file()
                && metadata.uid() == trusted_owner()
                && metadata.mode() & 0o022 == 0
                && parent.uid() == trusted_owner()
                && parent.mode() & 0o022 == 0
        },
    )
}

fn find_trusted_sudo() -> Option<PathBuf> {
    ["sudo", "sudo.exe"].iter().find_map(|name| trusted_program(name))
}

fn format_command(program: &str, args: &[impl AsRef<OsStr>]) -> String {
    std::iter::once(program.to_owned())
        .chain(args.iter().map(|arg| quote_arg(arg.as_ref())))
        .collect::<Vec<_>>()
        .join(" ")
}

fn format_command_with_env(programs: &[String], env: &[String], args: &[String]) -> String {
    programs
        .iter()
        .cloned()
        .chain(env.iter().cloned())
        .chain(args.iter().map(|arg| quote_arg(OsStr::new(arg))))
        .collect::<Vec<_>>()
        .join(" ")
}

fn quote_arg(arg: &OsStr) -> String {
    let text = arg.to_string_lossy();
    if text.chars().all(|c| c.is_ascii_alphanumeric() || "-_./:=@".contains(c)) {
        text.into_owned()
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}

fn config_path() -> Result<PathBuf, String> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("APPDATA").map(PathBuf::from))
        .or_else(|| home_dir().map(|home| home.join(".config")))
        .ok_or_else(missing("cannot determine home or config directory"))?;
    ensure_absolute(&base, "config directory")?;
    Ok(base.join("mnn").join("config"))
}

fn validate_user_file(path: &Path, label: &str) -> Result<(), String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!("cannot inspect {label} {}: {error}", path.display()));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!("{label} path is not a regular file: {}", path.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != effective_uid() {
            return Err(format!("{label} is not owned by the current user: {}", path.display()));
        }
        if metadata.mode() & 0o022 != 0 {
            return Err(format!("{label} is writable by group or others: {}", path.display()));
        }
    }
    Ok(())
}

fn ensure_user_directory(path: &Path, private: bool) -> Result<(), String> {
    #[cfg(not(unix))]
    let _ = private;
    let existed = match fs::symlink_metadata(path) {
        Ok(_) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(format!("cannot inspect directory {}: {error}", path.display()));
        }
    };
    if !existed {
        fs::create_dir_all(path)
            .map_err(describe(format!("cannot create directory {}", path.display())))?;
    }
    let metadata = fs::symlink_metadata(path)
        .map_err(describe(format!("cannot inspect directory {}", path.display())))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!("unsafe directory path: {}", path.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::fs::PermissionsExt;
        if metadata.uid() != effective_uid() {
            return Err(format!("directory is not owned by the current user: {}", path.display()));
        }
        let mode = metadata.mode() & 0o777;
        if private && !existed {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))
                .map_err(describe(format!("cannot secure directory {}", path.display())))?;
            return Ok(());
        }
        if mode & 0o022 != 0 || (private && mode & 0o077 != 0) {
            return Err(format!("directory permissions are unsafe: {}", path.display()));
        }
    }
    Ok(())
}

fn data_dir() -> Result<PathBuf, String> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("LOCALAPPDATA").map(PathBuf::from))
        .or_else(|| home_dir().map(|home| home.join(".local").join("share")))
        .ok_or_else(missing("cannot determine home or data directory"))?;
    ensure_absolute(&base, "data directory")?;
    Ok(base)
}

fn state_dir() -> Result<PathBuf, String> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("LOCALAPPDATA").map(PathBuf::from))
        .or_else(|| home_dir().map(|home| home.join(".local").join("state")))
        .ok_or_else(missing("cannot determine home or state directory"))?;
    ensure_absolute(&base, "state directory")?;
    Ok(base.join("mnn"))
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from)
}

fn ensure_absolute(path: &Path, label: &str) -> Result<(), String> {
    if !path.is_absolute()
        || path.components().any(|part| matches!(part, std::path::Component::ParentDir))
    {
        Err(format!("{label} must be absolute and contain no '..': {}", path.display()))
    } else {
        Ok(())
    }
}

fn open_log_file() -> Result<File, String> {
    let dir = state_dir()?;
    ensure_user_directory(&dir, true)?;
    let path = dir.join(format!("mnn-{}.log", unix_day()));
    validate_user_file(&path, "log file")?;
    #[cfg(unix)]
    if path.exists() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if fs::metadata(&path).map_err(describe("cannot inspect log file"))?.mode() & 0o077 != 0 {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
                .map_err(describe("cannot secure log file"))?;
        }
    }
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file =
        options.open(&path).map_err(describe(format!("cannot open log {}", path.display())))?;
    writeln!(file, "----- mnn run {} -----", unix_timestamp())
        .map_err(describe("cannot write log"))?;
    Ok(file)
}

fn acquire_run_lock() -> Result<File, String> {
    let directory = state_dir()?;
    ensure_user_directory(&directory, true)?;
    let path = directory.join("mnn.lock");
    validate_user_file(&path, "lock file")?;
    let mut options = OpenOptions::new();
    options.create(true).append(true).read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(&path)
        .map_err(describe(format!("cannot open lock file {}", path.display())))?;
    file.try_lock().map_err(describe(format!("another mnn run is active ({})", path.display())))?;
    Ok(file)
}

fn unix_timestamp() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs()
}

fn unix_day() -> u64 {
    unix_timestamp() / 86_400
}

fn generate_config() -> Result<(), String> {
    let path = config_path()?;
    if path.exists() {
        return Err(format!("config already exists: {}", path.display()));
    }
    let parent = path.parent().ok_or_else(missing("config path has no parent directory"))?;
    ensure_user_directory(parent, false)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .map_err(describe(format!("cannot create config {}", path.display())))?;
    file.write_all(
        b"targets=auto\nverbose=false\nsilent=false\nparallel=false\ndisable-log=false\n",
    )
    .map_err(describe(format!("cannot write config {}", path.display())))?;
    println!("Config generated: {}", path.display());
    Ok(())
}

fn print_completion(mode: CompletionMode) {
    match mode {
        CompletionMode::Nu => print!("{NUSHELL_COMPLETION}"),
        CompletionMode::All | CompletionMode::Detect => {
            eprintln!("select one shell: bash, fish, zsh, or nu");
        }
        mode => {
            let shell = completion_shell(mode).expect("shell mode is supported");
            let mut command = Cli::command();
            generate(shell, &mut command, "mnn", &mut io::stdout());
        }
    }
}

fn completion_shell(mode: CompletionMode) -> Option<Shell> {
    match mode {
        CompletionMode::Bash => Some(Shell::Bash),
        CompletionMode::Fish => Some(Shell::Fish),
        CompletionMode::Zsh => Some(Shell::Zsh),
        CompletionMode::Nu | CompletionMode::All | CompletionMode::Detect => None,
    }
}

enum InstallAction {
    Install,
    Reinstall,
    Update,
    Uninstall,
    Purge,
}

fn maintain(action: InstallAction, mode: Option<CompletionMode>) -> Result<(), String> {
    match action {
        InstallAction::Install | InstallAction::Reinstall | InstallAction::Update => {
            let binary = binary_install_path()?;
            let source = source_binary()?;
            validate_user_file(&source, "binary source")?;
            validate_user_file(&binary, "installed binary")?;
            if action_matches(&action, &binary, &source) {
                println!("Binary already installed: {}", binary.display());
            } else {
                install_file(&source, &binary, matches!(action, InstallAction::Install))?;
                println!("Binary installed: {}", binary.display());
            }
            install_completions(mode.unwrap_or(CompletionMode::Detect))?;
            #[cfg(not(windows))]
            atomic_write_file(&man_install_path()?, MAN_PAGE.as_bytes())?;
            if matches!(action, InstallAction::Install) && !config_path()?.exists() {
                generate_config()?;
            }
        }
        InstallAction::Uninstall | InstallAction::Purge => {
            let binary = binary_install_path()?;
            remove_file_if_present(&binary)?;
            for shell in [
                CompletionMode::Bash,
                CompletionMode::Fish,
                CompletionMode::Zsh,
                CompletionMode::Nu,
            ] {
                remove_file_if_present(&completion_path(shell)?)?;
            }
            #[cfg(not(windows))]
            remove_file_if_present(&man_install_path()?)?;
            if matches!(action, InstallAction::Purge) {
                remove_file_if_present(&config_path()?)?;
                let logs = state_dir()?;
                if fs::symlink_metadata(&logs).is_ok() {
                    ensure_user_directory(&logs, true)?;
                    fs::remove_dir_all(&logs)
                        .map_err(describe(format!("cannot remove logs {}", logs.display())))?;
                }
            }
        }
    }
    Ok(())
}

fn action_matches(action: &InstallAction, destination: &Path, source: &Path) -> bool {
    let same_path = destination == source
        || matches!(
            (fs::canonicalize(destination), fs::canonicalize(source)),
            (Ok(installed), Ok(current)) if installed == current
        );
    match action {
        InstallAction::Install => destination.exists(),
        InstallAction::Reinstall => same_path,
        InstallAction::Update => {
            if same_path {
                true
            } else {
                fs::read(destination).ok() == fs::read(source).ok()
            }
        }
        InstallAction::Uninstall | InstallAction::Purge => true,
    }
}

fn binary_install_path() -> Result<PathBuf, String> {
    let home = home_dir().ok_or_else(missing("cannot determine home directory"))?;
    let name = if cfg!(windows) { "mnn.exe" } else { "mnn" };
    Ok(home.join(".local").join("bin").join(name))
}

fn man_install_path() -> Result<PathBuf, String> {
    Ok(data_dir()?.join("man").join("man1").join("mnn.1"))
}

fn source_binary() -> Result<PathBuf, String> {
    if let Some(directory) = std::env::var_os("MNN_SOURCE_DIR") {
        let candidate =
            PathBuf::from(directory).join(if cfg!(windows) { "mnn.exe" } else { "mnn" });
        if candidate.is_file() {
            return Ok(candidate);
        }
        return Err(format!("MNN_SOURCE_DIR binary not found: {}", candidate.display()));
    }
    std::env::current_exe().map_err(describe("cannot resolve current executable"))
}

fn install_file(source: &Path, destination: &Path, preserve_existing: bool) -> Result<(), String> {
    validate_user_file(source, "binary source")?;
    let parent = destination.parent().ok_or_else(missing("install destination has no parent"))?;
    ensure_user_directory(parent, false)?;
    validate_user_file(destination, "install destination")?;
    if preserve_existing && destination.is_file() {
        return Ok(());
    }
    let mut input = File::open(source).map_err(describe("cannot open binary source"))?;
    let (temporary, mut output) = create_temporary_file(destination)?;
    io::copy(&mut input, &mut output).map_err(describe("cannot copy executable"))?;
    set_executable_permissions(&output)?;
    drop(output);
    replace_file(&temporary, destination)
}

fn atomic_write_file(destination: &Path, contents: &[u8]) -> Result<(), String> {
    ensure_user_directory(
        destination.parent().ok_or_else(missing("output path has no parent directory"))?,
        false,
    )?;
    validate_user_file(destination, "output file")?;
    let (temporary, mut output) = create_temporary_file(destination)?;
    output.write_all(contents).map_err(describe("cannot write temporary file"))?;
    drop(output);
    replace_file(&temporary, destination)
}

fn create_temporary_file(destination: &Path) -> Result<(PathBuf, File), String> {
    let parent = destination.parent().ok_or_else(missing("output path has no parent directory"))?;
    let name = destination
        .file_name()
        .ok_or_else(missing("output path has no file name"))?
        .to_string_lossy();
    for attempt in 0..100 {
        let temporary = parent.join(format!(".{name}.{}.{}.tmp", std::process::id(), attempt));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&temporary) {
            Ok(file) => return Ok((temporary, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("cannot create temporary file: {error}")),
        }
    }
    Err("could not create a unique temporary file".to_owned())
}

fn set_executable_permissions(file: &File) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o755))
            .map_err(describe("cannot set executable permissions"))?;
    }
    #[cfg(not(unix))]
    let _ = file;
    Ok(())
}

fn replace_file(temporary: &Path, destination: &Path) -> Result<(), String> {
    #[cfg(windows)]
    if destination.exists() {
        fs::remove_file(destination)
            .map_err(describe(format!("cannot replace {}", destination.display())))?;
    }
    fs::rename(temporary, destination)
        .map_err(describe(format!("cannot install {}", destination.display())))
}

fn remove_file_if_present(path: &Path) -> Result<(), String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("cannot inspect {}: {error}", path.display())),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!("refusing to remove unsafe path: {}", path.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != effective_uid() {
            return Err(format!(
                "refusing to remove a file not owned by the current user: {}",
                path.display()
            ));
        }
    }
    fs::remove_file(path).map_err(describe(format!("cannot remove {}", path.display())))
}

fn completion_path(mode: CompletionMode) -> Result<PathBuf, String> {
    let home = home_dir().ok_or_else(missing("cannot determine home directory"))?;
    let data = data_dir()?;
    Ok(match mode {
        CompletionMode::Bash => data.join("bash-completion").join("completions").join("mnn"),
        CompletionMode::Fish => data.join("fish").join("vendor_completions.d").join("mnn.fish"),
        CompletionMode::Zsh => data.join("zsh").join("site-functions").join("_mnn"),
        CompletionMode::Nu => std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"))
            .join("nushell")
            .join("completions")
            .join("mnn.nu"),
        CompletionMode::All | CompletionMode::Detect => {
            return Err("completion mode must name one shell".to_owned());
        }
    })
}

fn install_completions(mode: CompletionMode) -> Result<(), String> {
    let shells = match mode {
        CompletionMode::All => vec![
            CompletionMode::Bash,
            CompletionMode::Fish,
            CompletionMode::Zsh,
            CompletionMode::Nu,
        ],
        CompletionMode::Detect => ["bash", "fish", "zsh", "nu"]
            .into_iter()
            .filter_map(|name| {
                let command = if name == "nu" { "nu" } else { name };
                find_program(OsStr::new(command)).map(|_| match name {
                    "bash" => CompletionMode::Bash,
                    "fish" => CompletionMode::Fish,
                    "zsh" => CompletionMode::Zsh,
                    _ => CompletionMode::Nu,
                })
            })
            .collect(),
        one => vec![one],
    };
    for shell in shells {
        let path = completion_path(shell)?;
        let contents = completion_contents(shell);
        atomic_write_file(&path, &contents)
            .map_err(describe(format!("cannot install completion {}", path.display())))?;
        println!("{} completion installed: {}", completion_name(shell), path.display());
    }
    Ok(())
}

fn completion_contents(mode: CompletionMode) -> Vec<u8> {
    if mode == CompletionMode::Nu {
        return NUSHELL_COMPLETION.as_bytes().to_vec();
    }
    let mut output = Vec::new();
    let mut command = Cli::command();
    if let Some(shell) = completion_shell(mode) {
        generate(shell, &mut command, "mnn", &mut output);
    }
    output
}

fn completion_name(mode: CompletionMode) -> &'static str {
    match mode {
        CompletionMode::Bash => "Bash",
        CompletionMode::Fish => "Fish",
        CompletionMode::Zsh => "Zsh",
        CompletionMode::Nu => "Nushell",
        CompletionMode::All | CompletionMode::Detect => "Shell",
    }
}

#[cfg(test)]
mod tests;
