use super::*;
use std::io::Cursor;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::AtomicUsize;
use std::sync::{Condvar, mpsc};

static ENV_LOCK: Mutex<()> = Mutex::new(());
static COUNTER: AtomicUsize = AtomicUsize::new(0);

const VARIABLES: [&str; 14] = [
    "HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "APPDATA",
    "LOCALAPPDATA",
    "USERPROFILE",
    "PATH",
    "TERM",
    "NO_COLOR",
    "BUN_INSTALL",
    "PNPM_HOME",
    "MNN_SOURCE_DIR",
    "TERMUX_VERSION",
];

fn set(name: &str, value: impl AsRef<OsStr>) {
    unsafe { std::env::set_var(name, value) };
}

fn unset(name: &str) {
    unsafe { std::env::remove_var(name) };
}

struct Sandbox {
    _guard: MutexGuard<'static, ()>,
    root: PathBuf,
    saved: Vec<(&'static str, Option<OsString>)>,
}

impl Sandbox {
    fn new() -> Self {
        let guard = lock(&ENV_LOCK);
        let root = std::env::temp_dir().join(format!(
            "mnn-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        for name in ["home", "bin", "trusted"] {
            fs::create_dir_all(root.join(name)).unwrap();
            fs::set_permissions(root.join(name), fs::Permissions::from_mode(0o755)).unwrap();
        }
        let saved = VARIABLES.iter().map(|name| (*name, std::env::var_os(name))).collect();
        for name in VARIABLES {
            unset(name);
        }
        set("HOME", root.join("home"));
        set("PATH", root.join("bin"));
        *lock(&CAPTURE) = Some(Capture::default());
        *lock(&TRUSTED_OVERRIDE) = Some(vec![root.join("trusted")]);
        Self { _guard: guard, root, saved }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    fn home(&self) -> PathBuf {
        self.path("home")
    }

    fn script(&self, directory: &str, name: &str, body: &str) -> PathBuf {
        let path = self.path(directory).join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn bin(&self, name: &str, body: &str) -> PathBuf {
        self.script("bin", name, body)
    }

    fn trusted(&self, name: &str, body: &str) -> PathBuf {
        self.script("trusted", name, body)
    }

    fn fake_sudo(&self) {
        self.trusted("sudo", "case \"$1\" in -v) exit 0;; esac\nshift 2\nexec \"$@\"");
        self.trusted(
            "env",
            "while [ \"${1#*=}\" != \"$1\" ]; do export \"$1\"; shift; done\nexec \"$@\"",
        );
    }

    fn fake_system_managers(&self, body: &str) {
        for name in [
            "apk",
            "apt-get",
            "dnf",
            "emerge",
            "eopkg",
            "pacman",
            "pkg",
            "rpm-ostree",
            "snap",
            "determinate-nixd",
            "transactional-update",
            "xbps-install",
            "yum",
            "zypper",
        ] {
            self.trusted(name, body);
        }
    }

    fn user_file(&self, relative: &str, contents: &str, mode: u32) -> PathBuf {
        let path = self.path(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, contents).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    fn config(&self, contents: &str) -> PathBuf {
        let directory = self.home().join(".config").join("mnn");
        fs::create_dir_all(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.join("config");
        fs::write(&path, contents).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        path
    }

    fn out(&self) -> String {
        String::from_utf8_lossy(&lock(&CAPTURE).as_ref().unwrap().stdout).into_owned()
    }

    fn err(&self) -> String {
        String::from_utf8_lossy(&lock(&CAPTURE).as_ref().unwrap().stderr).into_owned()
    }

    fn fail_console(&self, fail: bool) {
        lock(&CAPTURE).as_mut().unwrap().fail = fail;
    }

    fn run(&self, args: &[&str]) -> Result<(), String> {
        let cli = Cli::try_parse_from(std::iter::once("mnn").chain(args.iter().copied())).unwrap();
        run(cli)
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        for (name, value) in &self.saved {
            match value {
                Some(value) => set(name, value),
                None => unset(name),
            }
        }
        *lock(&CAPTURE) = None;
        *lock(&TRUSTED_OVERRIDE) = None;
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn settings(silent: bool, verbose: bool) -> Settings {
    Settings { targets: vec![Target::Auto], silent, verbose, parallel: false, disable_log: true }
}

fn manager(show: bool, silent: bool, targets: &[Target]) -> ProgressManager {
    ProgressManager::with_size(show, silent, 0, targets, (80, 24))
}

fn full_log() -> Arc<Mutex<File>> {
    Arc::new(Mutex::new(OpenOptions::new().write(true).open("/dev/full").unwrap()))
}

fn file_log(path: &Path) -> Arc<Mutex<File>> {
    Arc::new(Mutex::new(OpenOptions::new().create(true).append(true).open(path).unwrap()))
}

fn step(program: &str, args: &[&str], system: bool) -> Step {
    Step::new("Running step...", program, args, system)
}

fn wait_for(mut condition: impl FnMut() -> bool) {
    let mut reached = false;
    for _ in 0..400 {
        reached = condition();
        if reached {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(reached, "condition not reached");
}

#[test]
fn expands_aliases_and_deduplicates_targets_in_canonical_order() {
    let _sandbox = Sandbox::new();
    let selected = expand_targets(&[Target::Debian, Target::Apt, Target::Apt]);
    assert_eq!(selected, vec![Target::Apt]);
}

#[test]
fn all_includes_windows_package_managers() {
    let _sandbox = Sandbox::new();
    let selected = expand_targets(&[Target::All]);
    assert!(selected.contains(&Target::Pip));
    assert!(selected.contains(&Target::Winget));
    assert!(selected.contains(&Target::Choco));
}

#[test]
fn auto_expands_to_the_detected_targets() {
    let sandbox = Sandbox::new();
    sandbox.bin("flatpak", "exit 0");
    assert_eq!(expand_targets(&[Target::Auto]), vec![Target::Flatpak]);
}

#[test]
fn pip_target_requires_an_installed_pip_command_or_module() {
    let _sandbox = Sandbox::new();
    let find = |program: &OsStr| Some(PathBuf::from(program));
    let interpreter = pip_interpreter_with(&find, &|program, _| program == "python3");
    assert_eq!(interpreter, Some(("python3".to_owned(), PathBuf::from("python3"))));

    assert!(
        pip_interpreter_with(&find, &|_, _| false).is_none(),
        "an interpreter without pip must not be detected as a pip target"
    );
}

#[test]
fn windows_targets_use_noninteractive_upgrade_commands() {
    let _sandbox = Sandbox::new();
    let winget = target_steps(Target::Winget, false);
    assert_eq!(winget[0].program, "winget");
    assert!(winget[0].args.iter().any(|arg| arg == "--accept-package-agreements"));
    let choco = target_steps(Target::Choco, false);
    assert_eq!(choco[0].program, "choco");
    assert_eq!(choco[0].args, ["upgrade", "all", "--yes", "--no-progress"].map(OsString::from));
}

#[test]
fn new_target_names_are_accepted_by_the_cli() {
    assert!(Cli::try_parse_from(["mnn", "pip"]).is_ok());
    assert!(Cli::try_parse_from(["mnn", "winget"]).is_ok());
    assert!(Cli::try_parse_from(["mnn", "choco"]).is_ok());
}

#[test]
fn config_boolean_values_are_case_insensitive() {
    assert!(parse_bool("parallel", "YES").unwrap());
    assert!(!parse_bool("parallel", "No").unwrap());
    assert!(parse_bool("parallel", "sometimes").is_err());
}

#[test]
fn command_arguments_are_quoted_without_shell_interpolation() {
    assert_eq!(
        format_command("manager", &["arg with spaces", "$(touch nope)"]),
        "manager 'arg with spaces' '$(touch nope)'"
    );
    assert_eq!(format_command("it's", &["it's"]), "it's 'it'\\''s'");
}

#[test]
fn completion_assets_are_available_for_every_legacy_shell() {
    assert!(!completion_contents(CompletionMode::Bash).is_empty());
    assert!(!completion_contents(CompletionMode::Fish).is_empty());
    assert!(!completion_contents(CompletionMode::Zsh).is_empty());
    assert!(completion_contents(CompletionMode::Nu).starts_with(b"def "));
    assert!(completion_contents(CompletionMode::All).is_empty());
    assert!(MAN_PAGE.contains("winget"));
}

#[test]
fn output_reader_preserves_carriage_return_progress_updates() {
    let sandbox = Sandbox::new();
    let progress = manager(false, true, &[Target::Pip]);
    let reader = spawn_output_reader(
        Box::new(Cursor::new(b"first\rsecond\r\nthird\n")),
        false,
        true,
        None,
        progress.bar(0),
        false,
    );
    assert_eq!(reader.join().unwrap().unwrap(), "first\rsecond\r\nthird\n");
    assert_eq!(sandbox.out(), "");
}

#[test]
fn output_reader_logs_lines_and_streams_to_the_right_console() {
    let sandbox = Sandbox::new();
    let path = sandbox.path("reader.log");
    let log = Some(file_log(&path));
    let progress = manager(false, false, &[Target::Pip]);
    for (is_stderr, bytes) in [(false, "a\r\nb\rc\n\n"), (true, "tail")] {
        let reader = spawn_output_reader(
            Box::new(Cursor::new(bytes.as_bytes().to_vec())),
            is_stderr,
            false,
            log.clone(),
            progress.bar(0),
            false,
        );
        reader.join().unwrap().unwrap();
    }
    assert_eq!(fs::read_to_string(&path).unwrap(), "a\nb\nc\ntail\n");
    assert_eq!(sandbox.out(), "a\r\nb\rc\n\n");
    assert_eq!(sandbox.err(), "tail");
}

#[test]
fn output_reader_filters_brew_download_progress() {
    let sandbox = Sandbox::new();
    let progress = manager(false, false, &[Target::Brew]);
    let input = b"Downloading [12]\rkeep\r\nDownloading [3]".to_vec();
    let reader = spawn_output_reader(
        Box::new(Cursor::new(input.clone())),
        false,
        false,
        None,
        progress.bar(0),
        true,
    );
    reader.join().unwrap().unwrap();
    assert_eq!(sandbox.out(), "keep\r\n");
    let reader = spawn_output_reader(
        Box::new(Cursor::new(b"partial line".to_vec())),
        true,
        false,
        None,
        progress.bar(0),
        true,
    );
    reader.join().unwrap().unwrap();
    assert_eq!(sandbox.err(), "partial line");
    let reader =
        spawn_output_reader(Box::new(Cursor::new(input)), false, true, None, progress.bar(0), true);
    reader.join().unwrap().unwrap();
    assert_eq!(sandbox.out(), "keep\r\n");
}

#[test]
fn output_reader_reports_console_and_log_failures() {
    let sandbox = Sandbox::new();
    let progress = manager(false, false, &[Target::Pip]);
    sandbox.fail_console(true);
    let reader = spawn_output_reader(
        Box::new(Cursor::new(b"text\n".to_vec())),
        false,
        false,
        None,
        progress.bar(0),
        false,
    );
    assert!(reader.join().unwrap().is_err());
    sandbox.fail_console(false);
    let reader = spawn_output_reader(
        Box::new(Cursor::new(b"text\n".to_vec())),
        false,
        true,
        Some(full_log()),
        progress.bar(0),
        false,
    );
    assert!(reader.join().unwrap().is_err());
}

#[test]
fn reader_threads_that_panic_are_reported() {
    let handle = std::thread::spawn(|| -> io::Result<String> { panic!("reader failed") });
    assert!(join_reader(handle).is_err());
    let handle = std::thread::spawn(|| Ok("text".to_owned()));
    assert_eq!(join_reader(handle).unwrap(), "text");
}

#[test]
fn brew_download_progress_filter_matches_only_numeric_counters() {
    assert!(is_brew_download_progress_line(b"Downloading [6891]"));
    assert!(!is_brew_download_progress_line(b"Downloading []"));
    assert!(!is_brew_download_progress_line(b"Downloading [6891/10000]"));
    assert!(!is_brew_download_progress_line(b"Downloading Homebrew API data"));
    assert!(!is_brew_download_progress_line(b"Nothing to update."));
}

#[test]
fn progress_frames_use_braille_patterns() {
    assert!(PROGRESS_FRAMES.iter().all(|frame| ('\u{2800}'..='\u{28ff}').contains(frame)));
}

#[test]
fn silent_progress_uses_footer_when_it_fits() {
    assert!(use_progress_footer(true, 24, 8, 0));
    assert!(use_progress_footer(true, 24, 1, 0));
    assert!(!use_progress_footer(true, 11, 8, 0));
    assert!(!use_progress_footer(false, 24, 8, 0));
    assert!(use_progress_footer(true, 13, 8, 1));
    assert!(!use_progress_footer(true, 12, 8, 1));
}

#[test]
fn silent_progress_rows_are_positioned_at_the_top() {
    assert_eq!(progress_terminal_row(0, 8, 24, true, 0), 1);
    assert_eq!(progress_terminal_row(7, 8, 24, true, 0), 8);
    assert_eq!(progress_terminal_row(0, 8, 24, false, 0), 17);
    assert_eq!(progress_terminal_row(7, 8, 24, false, 0), 24);
    assert_eq!(progress_terminal_row(0, 8, 24, true, 1), 2);
    assert_eq!(progress_terminal_row(7, 8, 24, true, 1), 9);
    assert_eq!(top_progress_next_row(8, 1, true), 10);
    assert_eq!(top_progress_next_row(1, 1, false), 3);
}

#[test]
fn sudo_notice_is_written_on_the_line_above_progress() {
    let mut output = Vec::new();
    write_top_notice(&mut output, "mnn: refreshing sudo credentials...").unwrap();
    assert_eq!(output, b"\x1b[1;1H\x1b[2Kmnn: refreshing sudo credentials...\r\n");
    assert_eq!(progress_terminal_row(0, 8, 24, true, 1), 2);
}

#[test]
fn progress_manager_new_uses_the_terminal_size_or_a_default() {
    let _sandbox = Sandbox::new();
    let progress = ProgressManager::new(false, false, 0, &[Target::Pip]);
    assert!(progress.inner.width > 0 && progress.inner.height > 0);
}

#[test]
fn parallel_targets_start_before_any_worker_finishes() {
    let _sandbox = Sandbox::new();
    let (started_tx, started_rx) = mpsc::channel();
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let worker_gate = Arc::clone(&gate);
    let worker = std::thread::spawn(move || {
        execute_targets(&[Target::Brew, Target::Pip], true, &|_, target| {
            started_tx.send(target).unwrap();
            let (released, ready) = &*worker_gate;
            let mut released = released.lock().unwrap();
            while !*released {
                released = ready.wait(released).unwrap();
            }
            TargetResult { target, status: Status::Ok, output: Vec::new() }
        })
    });

    let first_started = started_rx.recv_timeout(Duration::from_secs(2)).is_ok();
    let second_started = started_rx.recv_timeout(Duration::from_secs(2)).is_ok();
    let (released, ready) = &*gate;
    *released.lock().unwrap() = true;
    ready.notify_all();
    let results = worker.join().unwrap();

    assert!(first_started && second_started);
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].target, Target::Brew);
    assert_eq!(results[1].target, Target::Pip);
}

#[test]
fn a_panicking_worker_is_reported_as_failed() {
    let _sandbox = Sandbox::new();
    let results = execute_targets(&[Target::Brew, Target::Uv], true, &|_, target| {
        if target == Target::Brew {
            panic!("worker failed");
        }
        TargetResult { target, status: Status::Ok, output: Vec::new() }
    });
    assert!(matches!(&results[0].status, Status::Failed(reason) if reason.contains("panicked")));
    assert!(matches!(results[1].status, Status::Ok));
}

#[test]
fn sequential_execution_keeps_the_requested_order() {
    let _sandbox = Sandbox::new();
    let results = execute_targets(&[Target::Brew, Target::Uv], false, &|_, target| TargetResult {
        target,
        status: Status::Ok,
        output: Vec::new(),
    });
    assert_eq!(results[0].target, Target::Brew);
    assert_eq!(results[1].target, Target::Uv);
    let single = execute_targets(&[Target::Brew], true, &|index, target| TargetResult {
        target,
        status: Status::Ok,
        output: vec![index.to_string()],
    });
    assert_eq!(single[0].output, ["0"]);
}

#[test]
fn progress_status_symbols_keep_their_colors() {
    let _sandbox = Sandbox::new();
    for (status, symbol, label, color) in [
        (Status::Ok, '✓', "OK", "32"),
        (Status::Unavailable("not installed".to_owned()), '○', "UNAVAILABLE", "36"),
        (Status::Skipped("not selected".to_owned()), '⊘', "SKIP", "34"),
        (Status::Failed("command failed".to_owned()), '✗', "ERR", "31"),
    ] {
        let progress = manager(false, false, &[Target::Pip]);
        progress.bar(0).finish(Target::Pip, &status);
        let rows = lock(&progress.inner.rows);
        let row = &rows[0];
        assert_eq!(row.symbol, symbol);
        assert_eq!(row.status, label);
        let line = format_progress_line(1, 1, row, 80, true);
        assert!(line.contains(&format!("\x1b[{color}m{symbol}\x1b[0m")));
        assert!(line.contains(&format!("\x1b[{color}m{label}\x1b[0m")));
        assert_eq!(format_progress_line(1, 1, row, 80, false).chars().count(), 80);
    }
}

#[test]
fn waiting_and_updating_rows_are_formatted() {
    let _sandbox = Sandbox::new();
    let progress = manager(false, false, &[Target::Pip]);
    {
        let rows = lock(&progress.inner.rows);
        let waiting = format_progress_line(1, 1, &rows[0], 80, true);
        assert!(waiting.contains("WAITING"));
        assert!(!waiting.contains("\x1b[0m"));
    }
    let bar = progress.bar(0);
    bar.set_message("Updating pip...");
    let rows = lock(&progress.inner.rows);
    assert!(format_progress_line(1, 1, &rows[0], 80, true).contains("\x1b[33m"));
    assert!(format_progress_line(1, 1, &rows[0], 10, false).contains("UPDATING"));
}

#[test]
fn bars_outside_the_rows_are_ignored() {
    let sandbox = Sandbox::new();
    let progress = manager(true, false, &[Target::Pip]);
    let bar = progress.bar(7);
    bar.set_message("ignored");
    bar.finish(Target::Pip, &Status::Ok);
    assert!(!bar.is_hidden());
    drop(progress);
    assert!(sandbox.out().contains("WAITING"));
}

#[test]
fn bottom_footer_draws_rows_and_restores_the_screen() {
    let sandbox = Sandbox::new();
    let mut progress = manager(true, false, &[Target::Pip, Target::Uv]);
    assert!(progress.footer_active);
    let bar = progress.bar(0);
    bar.set_message("Updating pip...");
    bar.println("hello");
    bar.write_output(b"child", false).unwrap();
    bar.write_output(b"oops", true).unwrap();
    bar.finish(Target::Pip, &Status::Ok);
    progress.finish();
    progress.finish();
    let out = sandbox.out();
    assert!(out.contains("\x1b[2A\x1b7\x1b[1;22r"));
    assert!(out.contains("hello\n"));
    assert!(out.contains("child"));
    assert!(out.contains("Updated pip"));
    assert!(out.ends_with(
        "WAITING
"
    ));
    assert_eq!(sandbox.err(), "oops");
}

#[test]
fn silent_footer_stays_at_the_top() {
    let sandbox = Sandbox::new();
    let mut progress = ProgressManager::with_size(true, true, 1, &[Target::Pip], (80, 24));
    assert!(progress.footer_active && progress.inner.top);
    progress.bar(0).set_message("Updating pip...");
    progress.finish();
    let out = sandbox.out();
    assert!(out.contains("\x1b[3;24r\x1b[3;1H\x1b[?25l"));
    assert!(out.contains("\x1b[3;1H\x1b[?25h"));
}

#[test]
fn silent_progress_without_room_for_a_footer_uses_a_single_line() {
    let sandbox = Sandbox::new();
    let targets = [Target::Pip, Target::Uv, Target::Brew];
    let mut progress = ProgressManager::with_size(true, true, 0, &targets, (80, 5));
    assert!(!progress.footer_active && progress.inner.visible);
    let bar = progress.bar(1);
    bar.set_message("Updating uv...");
    bar.finish(Target::Uv, &Status::Ok);
    progress.bar(2).set_message("Updating brew...");
    progress.finish();
    let out = sandbox.out();
    assert!(out.contains("[2/3]"));
    assert!(out.contains("[3/3]"));
    assert!(out.ends_with("\x1b[2;1H\x1b[?25h"));
}

#[test]
fn hidden_progress_draws_nothing() {
    let sandbox = Sandbox::new();
    let mut progress = manager(true, false, &[Target::Pip; 30]);
    assert!(!progress.footer_active && !progress.inner.visible);
    let bar = progress.bar(0);
    assert!(bar.is_hidden());
    bar.set_message("Updating pip...");
    bar.println("line");
    progress.finish();
    assert_eq!(sandbox.out(), "line\n");
}

#[test]
fn draw_footer_ignores_empty_row_sets() {
    let sandbox = Sandbox::new();
    let progress = ProgressManager::with_size(true, true, 0, &[Target::Pip; 30], (80, 5));
    assert!(current_row(&[]).is_none());
    draw_footer(&progress.inner, &[]);
    assert_eq!(sandbox.out(), "");
}

#[test]
fn ticker_animates_running_rows() {
    let sandbox = Sandbox::new();
    let mut progress = manager(true, false, &[Target::Pip]);
    progress.bar(0).set_message("Updating pip...");
    let marker = PROGRESS_FRAMES[1].to_string();
    wait_for(|| {
        if let Some(ticker) = &progress.ticker {
            ticker.thread().unpark();
        }
        sandbox.out().contains(&marker)
    });
    progress.finish();
}

#[test]
fn idle_ticker_does_not_redraw() {
    let sandbox = Sandbox::new();
    let mut progress = manager(true, false, &[Target::Pip]);
    let before = sandbox.out().len();
    progress.ticker.as_ref().unwrap().thread().unpark();
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(sandbox.out().len(), before);
    progress.finish();
}

#[test]
fn colors_follow_no_color() {
    let _sandbox = Sandbox::new();
    assert!(manager(true, false, &[Target::Pip]).inner.colored);
    set("NO_COLOR", "1");
    assert!(!manager(true, false, &[Target::Pip]).inner.colored);
    assert!(!manager(false, false, &[Target::Pip]).inner.colored);
}

#[test]
fn console_failures_are_reported_to_callers() {
    let sandbox = Sandbox::new();
    sandbox.fail_console(true);
    assert!(emit(false, b"x").is_err());
    assert!(Console.write(b"x").is_err());
    sandbox.fail_console(false);
    assert_eq!(Console.write(b"ok").unwrap(), 2);
    Console.flush().unwrap();
    assert_eq!(sandbox.out(), "ok");
}

#[test]
fn console_writes_reach_the_real_streams_without_capture() {
    let _sandbox = Sandbox::new();
    *lock(&CAPTURE) = None;
    emit(false, b"").unwrap();
    emit(true, b"").unwrap();
    *lock(&CAPTURE) = Some(Capture::default());
}

#[test]
fn progress_is_wanted_only_on_interactive_terminals() {
    let term = || Some(OsString::from("xterm"));
    assert!(progress_wanted(false, true, term()));
    assert!(!progress_wanted(true, true, term()));
    assert!(!progress_wanted(false, false, term()));
    assert!(!progress_wanted(false, true, None));
    assert!(!progress_wanted(false, true, Some(OsString::from("dumb"))));
}

#[test]
fn root_is_refused() {
    assert!(ensure_not_root(0).is_err());
    assert!(ensure_not_root(1000).is_ok());
}

#[test]
fn missing_config_uses_defaults() {
    let _sandbox = Sandbox::new();
    let settings = load_settings().unwrap();
    assert_eq!(settings.targets, vec![Target::Auto]);
    assert!(!settings.silent && !settings.verbose && !settings.parallel && !settings.disable_log);
}

#[test]
fn config_file_overrides_every_setting() {
    let sandbox = Sandbox::new();
    sandbox.config(
        "// note\n\n# comment\ntargets = apt, brew\tuv # inline\nverbose=yes\nsilent=false\nparallel=1\ndisable-log=true\n",
    );
    let settings = load_settings().unwrap();
    assert_eq!(settings.targets, vec![Target::Apt, Target::Brew, Target::Uv]);
    assert!(settings.verbose && settings.parallel && settings.disable_log && !settings.silent);
}

#[test]
fn invalid_config_is_rejected_with_a_reason() {
    let sandbox = Sandbox::new();
    for (contents, expected) in [
        ("nonsense\n", "invalid config line 1"),
        ("targets=nope\n", "unknown target in config: nope"),
        ("targets=\n", "targets config cannot be empty"),
        ("flavor=mint\n", "unknown config setting: flavor"),
        ("verbose=maybe\n", "invalid config setting: verbose=maybe"),
    ] {
        sandbox.config(contents);
        let error = load_settings().unwrap_err();
        assert!(error.contains(expected), "{error}");
    }
}

#[test]
fn unreadable_or_unsafe_config_is_rejected() {
    let sandbox = Sandbox::new();
    let path = sandbox.config("targets=auto\n");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o200)).unwrap();
    assert!(load_settings().unwrap_err().contains("cannot read config"));
    fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
    assert!(load_settings().unwrap_err().contains("writable by group or others"));
}

#[test]
fn base_directories_follow_the_environment() {
    let sandbox = Sandbox::new();
    let home = sandbox.home();
    assert_eq!(config_path().unwrap(), home.join(".config/mnn/config"));
    assert_eq!(data_dir().unwrap(), home.join(".local/share"));
    assert_eq!(state_dir().unwrap(), home.join(".local/state/mnn"));
    set("XDG_CONFIG_HOME", sandbox.path("c"));
    set("XDG_DATA_HOME", sandbox.path("d"));
    set("XDG_STATE_HOME", sandbox.path("s"));
    assert_eq!(config_path().unwrap(), sandbox.path("c/mnn/config"));
    assert_eq!(data_dir().unwrap(), sandbox.path("d"));
    assert_eq!(state_dir().unwrap(), sandbox.path("s/mnn"));
    for name in ["XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME"] {
        unset(name);
    }
    set("APPDATA", sandbox.path("app"));
    set("LOCALAPPDATA", sandbox.path("local"));
    assert_eq!(config_path().unwrap(), sandbox.path("app/mnn/config"));
    assert_eq!(data_dir().unwrap(), sandbox.path("local"));
    assert_eq!(state_dir().unwrap(), sandbox.path("local/mnn"));
    unset("HOME");
    set("USERPROFILE", sandbox.path("profile"));
    assert_eq!(home_dir().unwrap(), sandbox.path("profile"));
    unset("USERPROFILE");
    assert!(home_dir().is_none());
}

#[test]
fn base_directories_require_a_home_and_absolute_paths() {
    let sandbox = Sandbox::new();
    unset("HOME");
    assert!(config_path().unwrap_err().contains("config directory"));
    assert!(data_dir().unwrap_err().contains("data directory"));
    assert!(state_dir().unwrap_err().contains("state directory"));
    assert!(binary_install_path().is_err());
    assert!(completion_path(CompletionMode::Bash).is_err());
    set("XDG_CONFIG_HOME", "relative");
    set("XDG_DATA_HOME", sandbox.path("a/../b"));
    set("XDG_STATE_HOME", "relative");
    assert!(config_path().unwrap_err().contains("must be absolute"));
    assert!(data_dir().unwrap_err().contains("must be absolute"));
    assert!(state_dir().unwrap_err().contains("must be absolute"));
}

#[test]
fn user_files_must_be_private_regular_files() {
    let sandbox = Sandbox::new();
    validate_user_file(&sandbox.path("missing"), "file").unwrap();
    let file = sandbox.user_file("file", "x", 0o600);
    validate_user_file(&file, "file").unwrap();
    let link = sandbox.path("link");
    std::os::unix::fs::symlink(&file, &link).unwrap();
    assert!(validate_user_file(&link, "file").unwrap_err().contains("not a regular file"));
    assert!(
        validate_user_file(&sandbox.path("home"), "file")
            .unwrap_err()
            .contains("not a regular file")
    );
    assert!(
        validate_user_file(&file.join("child"), "file")
            .unwrap_err()
            .contains("cannot inspect file")
    );
    assert!(
        validate_user_file(Path::new("/etc/passwd"), "file")
            .unwrap_err()
            .contains("not owned by the current user")
    );
    fs::set_permissions(&file, fs::Permissions::from_mode(0o620)).unwrap();
    assert!(validate_user_file(&file, "file").unwrap_err().contains("writable by group or others"));
}

#[test]
fn user_directories_are_created_privately_and_checked() {
    let sandbox = Sandbox::new();
    let created = sandbox.path("home/new/dir");
    ensure_user_directory(&created, true).unwrap();
    assert_eq!(fs::metadata(&created).unwrap().permissions().mode() & 0o777, 0o700);
    ensure_user_directory(&created, true).unwrap();
    ensure_user_directory(&sandbox.path("home/plain"), false).unwrap();
    fs::set_permissions(&created, fs::Permissions::from_mode(0o750)).unwrap();
    assert!(ensure_user_directory(&created, true).unwrap_err().contains("permissions are unsafe"));
    fs::set_permissions(&created, fs::Permissions::from_mode(0o770)).unwrap();
    assert!(ensure_user_directory(&created, false).is_err());
    let file = sandbox.user_file("file", "x", 0o600);
    assert!(ensure_user_directory(&file, false).unwrap_err().contains("unsafe directory path"));
    let link = sandbox.path("dirlink");
    std::os::unix::fs::symlink(sandbox.path("home"), &link).unwrap();
    assert!(ensure_user_directory(&link, false).unwrap_err().contains("unsafe directory path"));
    assert!(
        ensure_user_directory(&file.join("child"), false)
            .unwrap_err()
            .contains("cannot inspect directory")
    );
    assert!(
        ensure_user_directory(Path::new("/usr"), false)
            .unwrap_err()
            .contains("not owned by the current user")
    );
    let locked = sandbox.path("locked");
    fs::create_dir(&locked).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).unwrap();
    assert!(
        ensure_user_directory(&locked.join("child"), false)
            .unwrap_err()
            .contains("cannot create directory")
    );
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn log_files_are_private_and_appended_to() {
    let sandbox = Sandbox::new();
    let first = open_log_file().unwrap();
    drop(first);
    let directory = state_dir().unwrap();
    let path = directory.join(format!("mnn-{}.log", unix_day()));
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    open_log_file().unwrap();
    assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(fs::read_to_string(&path).unwrap().matches("----- mnn run").count(), 2);
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    assert!(open_log_file().unwrap_err().contains("not a regular file"));
    fs::remove_dir(&path).unwrap();
    unset("HOME");
    assert!(open_log_file().is_err());
    drop(sandbox);
}

#[test]
fn log_writes_report_failures() {
    let _sandbox = Sandbox::new();
    let log = full_log();
    assert!(write_log(&log, "text").unwrap_err().starts_with("could not write log"));
    assert!(append_logged_output_line(&Some(log), b"text").is_err());
    append_logged_output_line(&None, b"text").unwrap();
}

#[test]
fn only_one_run_may_hold_the_lock() {
    let sandbox = Sandbox::new();
    let first = acquire_run_lock().unwrap();
    assert!(acquire_run_lock().unwrap_err().contains("another mnn run is active"));
    drop(first);
    acquire_run_lock().unwrap();
    let lock_path = state_dir().unwrap().join("mnn.lock");
    fs::remove_file(&lock_path).unwrap();
    fs::create_dir(&lock_path).unwrap();
    assert!(acquire_run_lock().is_err());
    fs::remove_dir(&lock_path).unwrap();
    fs::set_permissions(state_dir().unwrap(), fs::Permissions::from_mode(0o500)).unwrap();
    assert!(acquire_run_lock().unwrap_err().contains("cannot open lock file"));
    fs::set_permissions(state_dir().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
    unset("HOME");
    assert!(acquire_run_lock().is_err());
    drop(sandbox);
}

#[test]
fn generated_config_is_private_and_not_overwritten() {
    let sandbox = Sandbox::new();
    generate_config().unwrap();
    let path = config_path().unwrap();
    assert!(fs::read_to_string(&path).unwrap().starts_with("targets=auto"));
    assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);

    assert!(generate_config().unwrap_err().contains("already exists"));
    load_settings().unwrap();
    fs::remove_file(&path).unwrap();
    let directory = path.parent().unwrap();
    fs::set_permissions(directory, fs::Permissions::from_mode(0o500)).unwrap();
    assert!(generate_config().unwrap_err().contains("cannot create config"));
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
    unset("HOME");
    assert!(generate_config().is_err());
    set("HOME", sandbox.home());
    set("XDG_CONFIG_HOME", Path::new("/proc/self"));
    assert!(generate_config().is_err());
}

#[test]
fn config_output_errors_are_reported() {
    let sandbox = Sandbox::new();
    set("XDG_CONFIG_HOME", sandbox.path("rootfile"));
    fs::write(sandbox.path("rootfile"), "x").unwrap();
    assert!(generate_config().is_err());
    assert_eq!(config_path().unwrap(), sandbox.path("rootfile").join("mnn/config"));
}

#[test]
fn completion_printing_covers_every_mode() {
    let sandbox = Sandbox::new();
    print_completion(CompletionMode::Nu);
    print_completion(CompletionMode::All);
    print_completion(CompletionMode::Detect);
    assert_eq!(completion_shell(CompletionMode::Zsh), Some(Shell::Zsh));
    assert_eq!(completion_shell(CompletionMode::Nu), None);
    for mode in [CompletionMode::Bash, CompletionMode::Fish, CompletionMode::Zsh] {
        print_completion(mode);
    }
    drop(sandbox);
}

#[test]
fn completion_names_and_paths_are_distinct() {
    let sandbox = Sandbox::new();
    let mut paths = BTreeSet::new();
    for (mode, name) in [
        (CompletionMode::Bash, "Bash"),
        (CompletionMode::Fish, "Fish"),
        (CompletionMode::Zsh, "Zsh"),
        (CompletionMode::Nu, "Nushell"),
    ] {
        assert_eq!(completion_name(mode), name);
        assert!(paths.insert(completion_path(mode).unwrap()));
    }
    assert_eq!(completion_name(CompletionMode::All), "Shell");
    assert!(completion_path(CompletionMode::All).is_err());
    assert!(completion_path(CompletionMode::Detect).is_err());
    set("XDG_CONFIG_HOME", sandbox.path("c"));
    assert_eq!(
        completion_path(CompletionMode::Nu).unwrap(),
        sandbox.path("c/nushell/completions/mnn.nu")
    );
}

#[test]
fn completions_install_for_selected_or_detected_shells() {
    let sandbox = Sandbox::new();
    install_completions(CompletionMode::Fish).unwrap();
    assert!(completion_path(CompletionMode::Fish).unwrap().is_file());
    install_completions(CompletionMode::Detect).unwrap();
    assert!(!completion_path(CompletionMode::Bash).unwrap().exists());
    for shell in ["bash", "fish", "zsh", "nu"] {
        sandbox.bin(shell, "exit 0");
    }
    install_completions(CompletionMode::Detect).unwrap();
    for mode in [CompletionMode::Bash, CompletionMode::Zsh, CompletionMode::Nu] {
        assert!(completion_path(mode).unwrap().is_file());
    }
    install_completions(CompletionMode::All).unwrap();

    fs::set_permissions(sandbox.home(), fs::Permissions::from_mode(0o500)).unwrap();
    fs::remove_dir_all(sandbox.home().join(".local")).ok();
    fs::set_permissions(sandbox.home(), fs::Permissions::from_mode(0o755)).unwrap();
    set("XDG_DATA_HOME", sandbox.path("file"));
    fs::write(sandbox.path("file"), "x").unwrap();
    assert!(
        install_completions(CompletionMode::Bash)
            .unwrap_err()
            .contains("cannot install completion")
    );
}

#[test]
fn atomic_writes_replace_files_and_report_failures() {
    let sandbox = Sandbox::new();
    let target = sandbox.path("home/out/file");
    atomic_write_file(&target, b"one").unwrap();
    atomic_write_file(&target, b"two").unwrap();
    assert_eq!(fs::read_to_string(&target).unwrap(), "two");
    assert!(atomic_write_file(Path::new("/"), b"x").is_err());
    fs::remove_file(&target).unwrap();
    fs::create_dir(&target).unwrap();
    assert!(atomic_write_file(&target, b"x").is_err());
    assert!(atomic_write_file(Path::new("/etc/passwd"), b"x").is_err());
}

#[test]
fn temporary_files_need_a_directory_name_and_free_slots() {
    let sandbox = Sandbox::new();
    assert!(create_temporary_file(Path::new("/")).is_err());
    assert!(create_temporary_file(&sandbox.path("home/..")).is_err());
    assert!(
        create_temporary_file(&sandbox.path("home/missing/file"))
            .unwrap_err()
            .contains("cannot create temporary file")
    );
    let destination = sandbox.path("home/busy");
    let (first, file) = create_temporary_file(&destination).unwrap();
    drop(file);
    assert!(first.is_file());
    let (second, _) = create_temporary_file(&destination).unwrap();
    assert_ne!(first, second);
    for attempt in 0..100 {
        let name = format!(".busy.{}.{}.tmp", std::process::id(), attempt);
        let _ = fs::write(sandbox.path("home").join(name), "");
    }
    assert!(create_temporary_file(&destination).unwrap_err().contains("unique temporary file"));
}

#[test]
fn files_install_atomically_and_keep_existing_ones_on_request() {
    let sandbox = Sandbox::new();
    let source = sandbox.user_file("src/mnn", "binary", 0o700);
    let destination = sandbox.path("home/bin/mnn");
    install_file(&source, &destination, false).unwrap();
    assert_eq!(fs::read_to_string(&destination).unwrap(), "binary");
    assert_eq!(fs::metadata(&destination).unwrap().permissions().mode() & 0o777, 0o755);
    fs::write(&source, "newer").unwrap();
    install_file(&source, &destination, true).unwrap();
    assert_eq!(fs::read_to_string(&destination).unwrap(), "binary");
    install_file(&source, &destination, false).unwrap();
    assert_eq!(fs::read_to_string(&destination).unwrap(), "newer");
    assert!(
        install_file(&sandbox.path("src/none"), &destination, false)
            .unwrap_err()
            .contains("cannot open binary source")
    );
    assert!(install_file(&source, Path::new("/"), false).is_err());
    assert!(install_file(&source, &sandbox.path("src"), false).is_err());
    let unreadable = sandbox.user_file("src/mem", "", 0o600);
    fs::remove_file(&unreadable).unwrap();
    std::os::unix::fs::symlink("/proc/self/mem", &unreadable).unwrap();
    assert!(install_file(&unreadable, &destination, false).is_err());
    let mem = Path::new("/proc/self/mem");
    assert!(
        install_file(mem, &sandbox.path("home/bin/copy"), false)
            .unwrap_err()
            .contains("cannot copy executable")
    );
}

#[test]
fn executable_permissions_report_failures() {
    let _sandbox = Sandbox::new();
    let proc = File::open("/proc/self/status").unwrap();
    assert!(
        set_executable_permissions(&proc)
            .unwrap_err()
            .contains("cannot set executable permissions")
    );
}

#[test]
fn replacing_a_file_reports_rename_failures() {
    let sandbox = Sandbox::new();
    let temporary = sandbox.user_file("home/tmp", "x", 0o600);
    let destination = sandbox.path("home/dest");
    fs::create_dir_all(destination.join("child")).unwrap();
    assert!(replace_file(&temporary, &destination).unwrap_err().contains("cannot install"));
}

#[test]
fn removal_refuses_unsafe_paths() {
    let sandbox = Sandbox::new();
    remove_file_if_present(&sandbox.path("home/missing")).unwrap();
    let file = sandbox.user_file("home/file", "x", 0o600);
    remove_file_if_present(&file).unwrap();
    assert!(!file.exists());
    assert!(remove_file_if_present(&sandbox.path("home")).unwrap_err().contains("unsafe path"));
    let target = sandbox.user_file("home/target", "x", 0o600);
    let link = sandbox.path("home/link");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert!(remove_file_if_present(&link).is_err());
    assert!(remove_file_if_present(&target.join("child")).unwrap_err().contains("cannot inspect"));
    assert!(
        remove_file_if_present(Path::new("/etc/passwd"))
            .unwrap_err()
            .contains("not owned by the current user")
    );
}

#[test]
fn install_paths_and_sources_follow_the_environment() {
    let sandbox = Sandbox::new();
    assert_eq!(binary_install_path().unwrap(), sandbox.home().join(".local/bin/mnn"));
    assert_eq!(man_install_path().unwrap(), sandbox.home().join(".local/share/man/man1/mnn.1"));
    assert_eq!(source_binary().unwrap(), std::env::current_exe().unwrap());
    set("MNN_SOURCE_DIR", sandbox.path("src"));
    assert!(source_binary().unwrap_err().contains("MNN_SOURCE_DIR binary not found"));
    let source = sandbox.user_file("src/mnn", "x", 0o700);
    assert_eq!(source_binary().unwrap(), source);
}

#[test]
fn actions_compare_the_installed_binary_with_the_source() {
    let sandbox = Sandbox::new();
    let source = sandbox.user_file("src/mnn", "same", 0o700);
    let copy = sandbox.user_file("dst/mnn", "same", 0o700);
    let other = sandbox.user_file("dst/other", "different", 0o700);
    let missing = sandbox.path("dst/missing");
    assert!(action_matches(&InstallAction::Install, &copy, &source));
    assert!(!action_matches(&InstallAction::Install, &missing, &source));
    assert!(action_matches(&InstallAction::Reinstall, &source, &source));
    assert!(!action_matches(&InstallAction::Reinstall, &copy, &source));
    assert!(action_matches(&InstallAction::Update, &source, &source));
    assert!(action_matches(&InstallAction::Update, &copy, &source));
    assert!(!action_matches(&InstallAction::Update, &other, &source));
    assert!(action_matches(&InstallAction::Uninstall, &other, &source));
    assert!(action_matches(&InstallAction::Purge, &other, &source));
}

#[test]
fn maintenance_installs_updates_and_removes_everything() {
    let sandbox = Sandbox::new();
    let source = sandbox.user_file("src/mnn", "v1", 0o700);
    set("MNN_SOURCE_DIR", source.parent().unwrap());
    let binary = binary_install_path().unwrap();

    maintain(InstallAction::Install, Some(CompletionMode::Bash)).unwrap();
    assert_eq!(fs::read_to_string(&binary).unwrap(), "v1");
    assert!(config_path().unwrap().is_file());
    assert!(man_install_path().unwrap().is_file());

    maintain(InstallAction::Install, None).unwrap();

    fs::write(&source, "v2").unwrap();
    maintain(InstallAction::Update, Some(CompletionMode::Fish)).unwrap();
    assert_eq!(fs::read_to_string(&binary).unwrap(), "v2");
    maintain(InstallAction::Update, None).unwrap();

    fs::write(&source, "v3").unwrap();
    maintain(InstallAction::Reinstall, None).unwrap();
    assert_eq!(fs::read_to_string(&binary).unwrap(), "v3");

    maintain(InstallAction::Uninstall, None).unwrap();
    assert!(!binary.exists());
    assert!(config_path().unwrap().is_file());
    assert!(!man_install_path().unwrap().exists());

    maintain(InstallAction::Uninstall, None).unwrap();
    maintain(InstallAction::Install, Some(CompletionMode::Nu)).unwrap();
    let _lock = open_log_file().unwrap();
    maintain(InstallAction::Purge, None).unwrap();
    assert!(!config_path().unwrap().exists());
    assert!(!state_dir().unwrap().exists());
    maintain(InstallAction::Purge, None).unwrap();
}

#[test]
fn maintenance_reports_missing_sources_and_unsafe_state() {
    let sandbox = Sandbox::new();
    set("MNN_SOURCE_DIR", sandbox.path("nowhere"));
    assert!(maintain(InstallAction::Install, None).is_err());
    unset("MNN_SOURCE_DIR");
    unset("HOME");
    assert!(maintain(InstallAction::Install, None).is_err());
    assert!(maintain(InstallAction::Purge, None).is_err());
    set("HOME", sandbox.home());
    let state = state_dir().unwrap();
    fs::create_dir_all(&state).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(maintain(InstallAction::Purge, None).is_err());
    let locked = sandbox.path("home/.local/state");
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).unwrap();
    assert!(maintain(InstallAction::Purge, None).unwrap_err().contains("cannot remove logs"));
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
    set("MNN_SOURCE_DIR", sandbox.path("src"));
    sandbox.user_file("src/mnn", "x", 0o700);
    fs::create_dir_all(binary_install_path().unwrap()).unwrap();
    assert!(maintain(InstallAction::Install, None).is_err());
}

#[test]
fn trusted_programs_must_be_root_like_and_not_writable() {
    let sandbox = Sandbox::new();
    let tool = sandbox.trusted("tool", "exit 0");
    assert_eq!(trusted_program("tool"), Some(tool.clone()));
    assert!(trusted_program_exists("tool"));
    assert!(!trusted_program_exists("absent"));
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o775)).unwrap();
    assert!(trusted_program("tool").is_none());
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink(sandbox.path("trusted/missing"), sandbox.path("trusted/dangling"))
        .unwrap();
    assert!(trusted_program("dangling").is_none());
    fs::create_dir(sandbox.path("trusted/folder")).unwrap();
    assert!(trusted_program("folder").is_none());
    fs::set_permissions(sandbox.path("trusted"), fs::Permissions::from_mode(0o775)).unwrap();
    assert!(trusted_program("tool").is_none());
    fs::set_permissions(sandbox.path("trusted"), fs::Permissions::from_mode(0o755)).unwrap();
    assert!(find_trusted_sudo().is_none());
    sandbox.fake_sudo();
    assert!(find_trusted_sudo().is_some());
    *lock(&TRUSTED_OVERRIDE) = None;
    assert_eq!(trusted_directories().len(), 6);
    assert!(trusted_program("mnn-test-missing").is_none());
}

#[test]
fn programs_are_found_on_the_path_when_executable() {
    let sandbox = Sandbox::new();
    assert!(find_program(OsStr::new("tool")).is_none());
    let tool = sandbox.bin("tool", "exit 0");
    assert_eq!(find_program(OsStr::new("tool")), Some(tool.clone()));
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(find_program(OsStr::new("tool")).is_none());
    assert!(!is_executable(&sandbox.path("bin/absent")));
    unset("PATH");
    assert!(find_program(OsStr::new("tool")).is_none());
}

#[test]
fn bun_and_pnpm_must_come_from_their_install_scripts() {
    let sandbox = Sandbox::new();
    assert!(!script_installed("bun", "BUN_INSTALL", ".bun"));
    sandbox.bin("bun", "exit 0");
    assert!(!script_installed("bun", "BUN_INSTALL", ".bun"));
    set("BUN_INSTALL", sandbox.path("bin"));
    assert!(script_installed("bun", "BUN_INSTALL", ".bun"));
    unset("BUN_INSTALL");
    unset("HOME");
    assert!(!script_installed("bun", "BUN_INSTALL", ".bun"));
    set("HOME", sandbox.path("."));
    set("PATH", sandbox.path("bin"));
    assert!(!script_installed("bun", "BUN_INSTALL", ".bun"));
    set("HOME", sandbox.root.join("bin/.."));
    assert!(!script_installed("bun", "BUN_INSTALL", "bin/.bun"));
    let installed = sandbox.path("home/.bun/bin");
    fs::create_dir_all(&installed).unwrap();
    let script = sandbox.script("home/.bun/bin", "bun", "exit 0");
    set("PATH", &installed);
    set("HOME", sandbox.home());
    assert!(script_installed("bun", "BUN_INSTALL", ".bun"));
    assert_eq!(script, installed.join("bun"));
}

#[test]
fn os_release_is_parsed_and_defaults_to_empty() {
    assert_eq!(
        parse_os_release("NAME=\"X\"\nID=\"Fedora\"\nID_LIKE=\"rhel centos\"\n"),
        ("fedora".to_owned(), "rhel centos".to_owned())
    );
    assert_eq!(parse_os_release(""), (String::new(), String::new()));
    let (id, _) = read_os_release();
    assert_eq!(id, id.to_ascii_lowercase());
    assert_eq!(zypper_action("opensuse-tumbleweed"), "dup");
    assert_eq!(zypper_action("opensuse-leap"), "update");
}

#[test]
fn termux_is_detected_from_the_environment() {
    let _sandbox = Sandbox::new();
    assert_eq!(is_termux(), Path::new("/data/data/com.termux").is_dir());
    set("TERMUX_VERSION", "0.118");
    assert!(is_termux());
    assert!(!Target::Apt.needs_sudo());
    let steps = target_steps(Target::Pkg, false);
    assert_eq!(steps.len(), 3);
    assert!(steps.iter().all(|step| !step.system));
    unset("TERMUX_VERSION");
}

#[test]
fn every_target_has_a_unique_name_and_sudo_policy() {
    let _sandbox = Sandbox::new();
    let mut names = BTreeSet::new();
    for target in Target::all().into_iter().chain([Target::Auto, Target::All, Target::Debian]) {
        assert!(names.insert(target.name()));
        assert_eq!(Target::from_str(target.name(), true).unwrap(), target);
    }
    assert!(Target::Apt.needs_sudo());
    assert!(!Target::Brew.needs_sudo());
}

#[test]
fn every_target_defines_its_commands() {
    let sandbox = Sandbox::new();
    for target in Target::all().into_iter().chain([Target::Debian, Target::Auto, Target::All]) {
        for verbose in [false, true] {
            let steps = target_steps(target, verbose);
            let expected_empty =
                matches!(target, Target::Aur | Target::Pip | Target::Auto | Target::All);
            assert_eq!(steps.is_empty(), expected_empty, "{}", target.name());
            for step in &steps {
                assert!(!step.description.is_empty() && !step.program.is_empty());
            }
        }
    }
    let quiet = target_steps(Target::Apt, false);
    assert_eq!(quiet[0].args, ["-qq", "update"].map(OsString::from));
    let loud = target_steps(Target::Debian, true);
    assert_eq!(loud[0].args, ["update"].map(OsString::from));
    assert_eq!(quiet[1].env.len(), 3);
    assert_eq!(target_steps(Target::Brew, false)[1].env.len(), 3);

    for (helper, extra) in [
        ("yay", "--answerclean"),
        ("paru", "--skipreview"),
        ("pikaur", "--noedit"),
        ("trizen", "--noconfirm"),
    ] {
        for name in ["yay", "paru", "pikaur", "trizen"] {
            let _ = fs::remove_file(sandbox.path("bin").join(name));
        }
        sandbox.bin(helper, "exit 0");
        let steps = target_steps(Target::Aur, false);
        assert_eq!(steps[0].program, helper);
        assert!(steps[0].args.iter().any(|arg| arg == extra));
    }

    sandbox.bin("bun", "exit 0");
    sandbox.bin("pnpm", "exit 0");
    assert!(target_steps(Target::Bun, false)[0].description.starts_with("Skipping bun"));
    assert!(target_steps(Target::Pnpm, false)[0].description.starts_with("Skipping pnpm"));
    set("BUN_INSTALL", sandbox.path("bin"));
    set("PNPM_HOME", sandbox.path("bin"));
    assert!(target_steps(Target::Bun, false)[0].description.starts_with("Upgrading"));
    assert!(target_steps(Target::Pnpm, false)[0].description.starts_with("Upgrading"));
}

#[test]
fn detection_follows_the_installed_commands_and_distribution() {
    let sandbox = Sandbox::new();
    for name in [
        "apt",
        "rpm-ostree",
        "transactional-update",
        "dnf",
        "yum",
        "zypper",
        "pacman",
        "paru",
        "xbps-install",
        "apk",
        "emerge",
        "eopkg",
        "brew",
        "snap",
        "flatpak",
        "soar",
        "determinate-nixd",
        "uv",
        "bun",
        "pnpm",
    ] {
        sandbox.bin(name, "exit 0");
    }
    sandbox.bin("python3", "[ \"$*\" = \"-m pip --version\" ]");
    set("BUN_INSTALL", sandbox.path("bin"));
    set("PNPM_HOME", sandbox.path("bin"));
    let everything = detect_targets_for("", "");
    for target in [
        Target::Apt,
        Target::RpmOstree,
        Target::TransactionalUpdate,
        Target::Pacman,
        Target::Aur,
        Target::Xbps,
        Target::Apk,
        Target::Emerge,
        Target::Eopkg,
        Target::Brew,
        Target::Snap,
        Target::Flatpak,
        Target::Soar,
        Target::Determinate,
        Target::Uv,
        Target::Bun,
        Target::Pnpm,
        Target::Pip,
    ] {
        assert!(everything.contains(&target), "{}", target.name());
    }
    assert!(!everything.contains(&Target::Dnf));
    assert!(detect_targets_for("ubuntu", "debian").contains(&Target::Apt));
    assert!(!detect_targets_for("ubuntu", "debian").contains(&Target::Pacman));
    assert!(detect_targets_for("endeavouros", "arch").contains(&Target::Pacman));

    fs::remove_file(sandbox.path("bin/rpm-ostree")).unwrap();
    fs::remove_file(sandbox.path("bin/transactional-update")).unwrap();
    assert!(detect_targets_for("fedora", "").contains(&Target::Dnf));
    assert!(!detect_targets_for("fedora", "").contains(&Target::Yum));
    assert!(detect_targets_for("opensuse-leap", "suse").contains(&Target::Zypper));
    fs::remove_file(sandbox.path("bin/dnf")).unwrap();
    assert!(detect_targets_for("rhel", "").contains(&Target::Yum));
    let generic = detect_targets_for("zzz", "zzz");
    assert!(generic.contains(&Target::Brew) && generic.contains(&Target::Pip));
    assert!(!generic.contains(&Target::Pacman));
    assert!(!detect_targets().is_empty());
}

#[test]
fn pip_detection_runs_the_matching_version_command() {
    let sandbox = Sandbox::new();
    assert!(!pip_available());
    sandbox.bin("python", "[ \"$*\" = \"-m pip --version\" ]");
    assert!(pip_available());
    fs::remove_file(sandbox.path("bin/python")).unwrap();
    sandbox.bin("py", "[ \"$*\" = \"-3 -m pip --version\" ]");
    assert!(pip_available());
    fs::remove_file(sandbox.path("bin/py")).unwrap();
    sandbox.bin("pip", "[ \"$*\" = \"--version\" ]");
    assert!(pip_available());
    let broken = sandbox.path("bin/pip");
    fs::write(&broken, "#!/nonexistent/interpreter\n").unwrap();
    fs::set_permissions(&broken, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(!pip_available());
}

#[test]
fn commands_are_formatted_for_display() {
    assert_eq!(
        format_command_with_env(
            &["sudo".to_owned(), "apt".to_owned()],
            &["A=1".to_owned()],
            &["it's".to_owned(), "plain".to_owned()]
        ),
        "sudo apt A=1 'it'\\''s' plain"
    );
    assert_eq!(quote_arg(OsStr::new("a-b_c.d/e:f=g@h")), "a-b_c.d/e:f=g@h");
    assert_eq!(quote_arg(OsStr::new("a b")), "'a b'");
    let output = Output {
        status: std::process::ExitStatus::default(),
        stdout: b"out".to_vec(),
        stderr: b"err".to_vec(),
    };
    assert_eq!(output_text(&output), "out\nerr");
    let output = Output {
        status: std::process::ExitStatus::default(),
        stdout: b"out\n".to_vec(),
        stderr: Vec::new(),
    };
    assert_eq!(output_text(&output), "out\n");
    let output = Output {
        status: std::process::ExitStatus::default(),
        stdout: Vec::new(),
        stderr: b"err".to_vec(),
    };
    assert_eq!(output_text(&output), "err");
}

#[test]
fn helper_closures_describe_their_errors() {
    assert_eq!(missing("gone")(), "gone");
    assert_eq!(describe::<io::Error>("context")(io::Error::other("boom")), "context: boom");
    let poisoned = Arc::new(Mutex::new(1));
    let clone = Arc::clone(&poisoned);
    let _ = std::thread::spawn(move || {
        let _guard = clone.lock().unwrap();
        panic!("poison");
    })
    .join();
    assert_eq!(*lock(&poisoned), 1);
}

#[test]
fn steps_carry_their_environment() {
    let step = Step::new("Do", "tool", &["a", "b"], true).with_env(&[("K", "V")]);
    assert_eq!(step.env, vec![(OsString::from("K"), OsString::from("V"))]);
    assert!(step.system);
    assert!(step_needs_sudo(&step));
}

fn progress_for(target: Target) -> ProgressManager {
    manager(false, false, &[target])
}

#[test]
fn user_steps_stream_output_and_log_it() {
    let sandbox = Sandbox::new();
    sandbox.bin("tool", "echo out; echo err >&2; exit 0");
    let log_path = sandbox.path("run.log");
    let log = file_log(&log_path);
    let progress = progress_for(Target::Uv);
    let result = execute_step(
        &step("tool", &["x", "y z"], false),
        false,
        &settings(false, true),
        Some(&log),
        &progress.bar(0),
    );
    assert_eq!(result.unwrap(), "");
    assert!(sandbox.out().contains("out"));
    let text = fs::read_to_string(&log_path).unwrap();
    assert!(text.contains("----- output: tool x 'y z' -----"));
    assert!(text.contains("out") && text.contains("err"));
}

#[test]
fn failing_steps_report_their_output_unless_silent() {
    let sandbox = Sandbox::new();
    sandbox.bin("both", "echo out; echo err >&2; exit 3");
    sandbox.bin("quiet", "exit 4");
    let progress = progress_for(Target::Uv);
    let run = |program: &str, silent: bool| {
        execute_step(
            &step(program, &[], false),
            false,
            &settings(silent, false),
            None,
            &progress.bar(0),
        )
        .unwrap_err()
    };
    let loud = run("both", false);
    assert!(loud.contains("exit status: 3: out"), "{loud}");
    assert!(loud.contains("err"));
    assert!(!run("both", true).contains("out"));
    assert!(run("quiet", false).ends_with("exit status: 4"));
    assert!(sandbox.out().contains("out"));
}

#[test]
fn only_stdout_or_only_stderr_is_kept_as_error_detail() {
    let sandbox = Sandbox::new();
    sandbox.bin("onlyout", "echo out; exit 1");
    sandbox.bin("onlyerr", "echo err >&2; exit 1");
    let progress = progress_for(Target::Uv);
    for (program, expected) in [("onlyout", "out"), ("onlyerr", "err")] {
        let error = execute_step(
            &step(program, &[], false),
            false,
            &settings(true, false),
            None,
            &progress.bar(0),
        )
        .unwrap_err();
        assert!(error.ends_with("exit status: 1"));
        let error = execute_step(
            &step(program, &[], false),
            false,
            &settings(false, false),
            None,
            &progress.bar(0),
        )
        .unwrap_err();
        assert!(error.ends_with(expected), "{error}");
    }
}

#[test]
fn steps_that_cannot_start_are_reported() {
    let sandbox = Sandbox::new();
    let broken = sandbox.path("bin/broken");
    fs::write(&broken, "#!/nonexistent/interpreter\n").unwrap();
    fs::set_permissions(&broken, fs::Permissions::from_mode(0o755)).unwrap();
    let progress = progress_for(Target::Uv);
    let error = execute_step(
        &step("broken", &[], false),
        false,
        &settings(false, false),
        None,
        &progress.bar(0),
    )
    .unwrap_err();
    assert!(error.starts_with("Running step...:"));
    let error = execute_step(
        &step("missing", &[], false),
        false,
        &settings(false, false),
        None,
        &progress.bar(0),
    )
    .unwrap_err();
    assert!(error.contains("not found or is not trusted"));
    let error = execute_step(
        &step("tool", &[], false),
        false,
        &settings(false, false),
        Some(&full_log()),
        &progress.bar(0),
    );
    assert!(error.is_err());
    sandbox.bin("tool", "exit 0");
    let error = execute_step(
        &step("tool", &[], false),
        false,
        &settings(false, false),
        Some(&full_log()),
        &progress.bar(0),
    )
    .unwrap_err();
    assert!(error.contains("could not write log"));
}

#[test]
fn system_steps_run_through_sudo_and_env() {
    let sandbox = Sandbox::new();
    sandbox.fake_sudo();
    sandbox.trusted("manager", "echo \"manager $* $MARK\"; exit 0");
    let progress = progress_for(Target::Apt);
    let plain = Step::new("Plain", "manager", &["a"], true);
    let with_env = Step::new("Env", "manager", &["b"], true).with_env(&[("MARK", "set")]);
    for step in [&plain, &with_env] {
        let result = execute_step(step, false, &settings(false, false), None, &progress.bar(0));
        assert_eq!(result.unwrap(), "");
    }
    assert!(sandbox.out().contains("manager a"));
    assert!(sandbox.out().contains("manager b set"));
    let dry = execute_step(&with_env, true, &settings(false, false), None, &progress.bar(0));
    assert_eq!(dry.unwrap(), "Env sudo manager MARK=set b");
    sandbox.bin("manager", "exit 0");
    let dry = execute_step(
        &Step::new("User", "manager", &["c d"], false),
        true,
        &settings(false, false),
        None,
        &progress.bar(0),
    );
    assert!(dry.unwrap().starts_with("User manager"));
}

#[test]
fn system_steps_need_trusted_helpers() {
    let sandbox = Sandbox::new();
    let progress = progress_for(Target::Apt);
    let attempt = |step: &Step| {
        execute_step(step, false, &settings(false, false), None, &progress.bar(0)).unwrap_err()
    };
    let plain = Step::new("Plain", "manager", &["a"], true);
    assert!(attempt(&plain).contains("not found or is not trusted"));
    sandbox.trusted("manager", "exit 0");
    assert_eq!(attempt(&plain), "no trusted sudo executable found");
    sandbox.trusted("sudo", "exit 0");
    let with_env = Step::new("Env", "manager", &["a"], true).with_env(&[("K", "V")]);
    assert_eq!(attempt(&with_env), "no trusted env executable found");
}

#[test]
fn zypper_exit_codes_have_special_meaning() {
    let sandbox = Sandbox::new();
    let progress = manager(false, false, &[Target::Zypper]);
    let zypper = Step::new("Zypper", "zypper", &["--non-interactive", "update"], false);
    let run = |silent: bool, log: Option<&Arc<Mutex<File>>>| {
        execute_step(&zypper, false, &settings(silent, false), log, &progress.bar(0))
    };
    sandbox.bin("zypper", "exit 102");
    assert_eq!(run(false, None).unwrap(), "");
    assert_eq!(run(true, None).unwrap(), "");
    let log_path = sandbox.path("zypper.log");
    let log = file_log(&log_path);
    assert_eq!(run(false, Some(&log)).unwrap(), "");
    assert!(fs::read_to_string(&log_path).unwrap().contains("reboot is required"));
    assert!(run(false, Some(&full_log())).unwrap_err().contains("could not write log"));
    let visible = ProgressManager::with_size(true, false, 0, &[Target::Zypper], (80, 24));
    execute_step(&zypper, false, &settings(false, false), None, &visible.bar(0)).unwrap();
    assert!(sandbox.out().contains("reboot is required"));

    let marker = sandbox.path("marker");
    sandbox.bin(
        "zypper",
        &format!("if [ -f {0} ]; then exit 0; else : > {0}; exit 103; fi", marker.display()),
    );
    assert_eq!(run(false, Some(&log)).unwrap(), "");
    assert!(fs::read_to_string(&log_path).unwrap().contains("retrying the update"));
    fs::remove_file(&marker).unwrap();
    assert!(run(false, Some(&full_log())).unwrap_err().contains("could not write log"));
    sandbox.bin("zypper", "exit 103");
    assert!(run(false, None).unwrap_err().contains("exit status: 103"));
    sandbox.bin("zypper", "exit 1");
    assert!(run(false, None).unwrap_err().contains("exit status: 1"));
}

#[test]
fn brew_update_hides_download_counters() {
    let sandbox = Sandbox::new();
    sandbox.bin("brew", "printf 'Downloading [1]\\rUpdated 2 taps\\n'");
    let progress = progress_for(Target::Brew);
    let update = Step::new("Brew", "brew", &["update"], false);
    execute_step(&update, false, &settings(false, false), None, &progress.bar(0)).unwrap();
    assert!(sandbox.out().contains("Updated 2 taps"));
    assert!(!sandbox.out().contains("Downloading"));
}

#[test]
fn pip_reports_missing_python() {
    let _sandbox = Sandbox::new();
    let progress = progress_for(Target::Pip);
    let mut output = Vec::new();
    let status = run_pip(&settings(false, false), false, &mut output, None, &progress.bar(0));
    assert!(matches!(status, Status::Unavailable(_)));
}

#[test]
fn pip_dry_run_lists_the_commands() {
    let sandbox = Sandbox::new();
    for (program, prefix) in [("pip", ""), ("python3", "-m pip "), ("py", "-3 -m pip ")] {
        for name in ["pip", "pip3", "python", "python3", "py"] {
            let _ = fs::remove_file(sandbox.path("bin").join(name));
        }
        sandbox.bin(program, "exit 0");
        let progress = progress_for(Target::Pip);
        let mut output = Vec::new();
        let status = run_pip(&settings(false, false), true, &mut output, None, &progress.bar(0));
        assert!(matches!(status, Status::Ok));
        assert!(output[0].contains(&format!("{program} {prefix}list --outdated")), "{output:?}");
        assert!(output[1].contains(&format!("{program} {prefix}install --upgrade '<package>'")));
    }
}

#[test]
fn pip_upgrades_every_outdated_package() {
    let sandbox = Sandbox::new();
    sandbox.bin(
        "python3",
        "case \"$*\" in\n\"-m pip --version\") exit 0;;\n\"-m pip list --outdated --format=json\") echo '[{\"name\":\"alpha\"},{\"version\":\"1\"},{\"name\":\"quiet\"},{\"name\":\"beta\"}]';;\n\"-m pip install --upgrade quiet\") exit 0;;\n\"-m pip install --upgrade alpha\") echo upgraded alpha;;\n\"-m pip install --upgrade beta\") echo upgraded beta;;\nesac",
    );
    let progress = progress_for(Target::Pip);
    let mut output = Vec::new();
    let status = run_pip(&settings(false, false), false, &mut output, None, &progress.bar(0));
    assert!(matches!(status, Status::Ok), "{status:?}");
    assert!(sandbox.out().contains("upgraded alpha"));
    assert!(sandbox.out().contains("upgraded beta"));
}

#[test]
fn pip_failures_are_classified() {
    let sandbox = Sandbox::new();
    let progress = progress_for(Target::Pip);
    let run = |silent: bool, log: Option<&Arc<Mutex<File>>>| {
        let mut output = Vec::new();
        run_pip(&settings(silent, false), false, &mut output, log, &progress.bar(0))
    };
    let script = |list: &str, install: &str| {
        sandbox.bin(
            "python3",
            &format!(
                "case \"$*\" in\n\"-m pip --version\") exit 0;;\n\"-m pip list --outdated --format=json\") {list};;\n*) {install};;\nesac"
            ),
        );
    };

    script("echo 'No module named pip' >&2; exit 1", "exit 0");
    assert!(matches!(run(false, None), Status::Unavailable(_)));

    script("echo boom; exit 2", "exit 0");
    assert!(matches!(run(false, None), Status::Failed(reason) if reason.contains("boom")));
    assert!(matches!(run(true, None), Status::Failed(reason) if !reason.contains("boom")));

    script("exit 2", "exit 0");
    assert!(
        matches!(run(false, None), Status::Failed(reason) if reason.ends_with("exit status: 2"))
    );

    let log = file_log(&sandbox.path("pip.log"));
    script("echo boom; exit 2", "exit 0");
    assert!(matches!(run(false, Some(&log)), Status::Failed(_)));
    assert!(
        matches!(run(false, Some(&full_log())), Status::Failed(reason) if reason.contains("could not write log"))
    );

    script("echo 'not json'", "exit 0");
    assert!(
        matches!(run(false, None), Status::Failed(reason) if reason.contains("invalid package data"))
    );

    script("echo '[{\"name\":\"alpha\"}]'", "echo nope >&2; exit 1");
    assert!(matches!(run(false, None), Status::Failed(reason) if reason.contains("nope")));

    sandbox.bin(
        "python3",
        "case \"$*\" in\n\"-m pip --version\") printf '#!/nonexistent/interpreter\\n' > \"$0\"; exit 0;;\nesac",
    );
    let mut output = Vec::new();
    let status = run_pip(&settings(false, false), false, &mut output, None, &progress.bar(0));
    assert!(
        matches!(status, Status::Failed(reason) if reason.contains("could not run pip")),
        "{output:?}"
    );
}

#[test]
fn keepalive_refreshes_sudo_until_dropped() {
    let sandbox = Sandbox::new();
    let marker = sandbox.path("refreshed");
    let sudo = sandbox.bin("fake-sudo", &format!(": > {}", marker.display()));
    let keepalive = SudoKeepalive::start(sudo);
    wait_for(|| {
        keepalive.thread.as_ref().unwrap().thread().unpark();
        marker.exists()
    });
    drop(keepalive);
}

#[test]
fn sudo_is_only_prepared_when_a_target_needs_it() {
    let sandbox = Sandbox::new();
    let quiet = settings(false, false);
    assert!(prepare_sudo_for_targets(&[Target::Apt], &quiet, true, false).unwrap().is_none());
    assert!(prepare_sudo_for_targets(&[Target::Brew], &quiet, false, false).unwrap().is_none());
    assert!(
        prepare_sudo_for_targets(&[Target::Apt], &quiet, false, false)
            .err()
            .unwrap()
            .contains("no trusted sudo binary")
    );
    sandbox.fake_sudo();
    for (verbose, reserve) in [(false, false), (true, false), (false, true), (true, true)] {
        let keepalive =
            prepare_sudo_for_targets(&[Target::Apt], &settings(false, verbose), false, reserve)
                .unwrap();
        assert!(keepalive.is_some());
    }
    assert!(sandbox.out().contains("refreshing sudo credentials"));
    assert!(sandbox.out().contains("Refreshing sudo credentials"));
    sandbox.fail_console(true);
    assert!(
        prepare_sudo_for_targets(&[Target::Apt], &quiet, false, true)
            .err()
            .unwrap()
            .contains("could not display sudo prompt")
    );
    sandbox.fail_console(false);
    sandbox.trusted("sudo", "exit 1");
    assert_eq!(
        prepare_sudo_for_targets(&[Target::Apt], &quiet, false, false).err().unwrap(),
        "sudo authentication failed"
    );
    let broken = sandbox.path("trusted/sudo");
    fs::write(&broken, "#!/nonexistent/interpreter\n").unwrap();
    assert!(
        prepare_sudo_for_targets(&[Target::Apt], &quiet, false, false)
            .err()
            .unwrap()
            .contains("could not start sudo")
    );
}

#[test]
fn sudo_need_follows_targets_and_aur_helpers() {
    let sandbox = Sandbox::new();
    let quiet = settings(false, false);
    assert!(!targets_need_sudo(&[Target::Brew], &quiet));
    assert!(targets_need_sudo(&[Target::Brew, Target::Apt], &quiet));
    assert!(!targets_need_sudo(&[Target::Aur], &quiet));
    sandbox.bin("yay", "exit 0");
    assert!(targets_need_sudo(&[Target::Aur], &quiet));
    set("TERMUX_VERSION", "1");
    assert!(!targets_need_sudo(&[Target::Apt], &quiet));
}

fn run_one(
    target: Target,
    settings: &Settings,
    dry_run: bool,
    log: Option<&Arc<Mutex<File>>>,
) -> TargetResult {
    let progress = manager(false, false, &[target]);
    let lane = Mutex::new(());
    run_target(target, settings, dry_run, &lane, log.cloned(), &progress.bar(0))
}

#[test]
fn bun_and_pnpm_targets_need_their_install_scripts() {
    let sandbox = Sandbox::new();
    let quiet = settings(false, false);
    for target in [Target::Bun, Target::Pnpm] {
        let result = run_one(target, &quiet, false, None);
        assert!(matches!(result.status, Status::Unavailable(_)));
        let progress = progress_for(target);
        let mut result = TargetResult { target, status: Status::Ok, output: Vec::new() };
        run_target_inner(target, &quiet, false, &mut result, None, &progress.bar(0));
        assert!(matches!(result.status, Status::Unavailable(_)));
    }
    sandbox.bin("bun", "exit 0");
    sandbox.bin("pnpm", "exit 0");
    set("BUN_INSTALL", sandbox.path("bin"));
    set("PNPM_HOME", sandbox.path("bin"));
    for target in [Target::Bun, Target::Pnpm] {
        assert!(matches!(run_one(target, &quiet, false, None).status, Status::Ok));
    }
}

#[test]
fn aur_needs_a_helper() {
    let sandbox = Sandbox::new();
    let quiet = settings(false, false);
    assert!(matches!(run_one(Target::Aur, &quiet, false, None).status, Status::Unavailable(_)));
    sandbox.bin("paru", "exit 0");
    assert!(matches!(run_one(Target::Aur, &quiet, false, None).status, Status::Ok));
}

#[test]
fn system_targets_need_trusted_commands() {
    let sandbox = Sandbox::new();
    let quiet = settings(false, false);
    let result = run_one(Target::Apt, &quiet, false, None);
    assert!(matches!(&result.status, Status::Unavailable(reason) if reason.contains("trusted")));
    let progress = progress_for(Target::Apt);
    let mut result = TargetResult { target: Target::Apt, status: Status::Ok, output: Vec::new() };
    run_target_inner(Target::Apt, &quiet, false, &mut result, None, &progress.bar(0));
    assert!(matches!(&result.status, Status::Unavailable(reason) if reason.contains("apt-get")));
    sandbox.fake_sudo();
    sandbox.fake_system_managers("exit 0");
    let result = run_one(Target::Apt, &quiet, false, None);
    assert!(matches!(result.status, Status::Ok), "{:?}", result.status);
    let result = run_one(Target::Apt, &quiet, true, None);
    assert!(result.output.iter().any(|line| line.contains("sudo apt-get")));
}

#[test]
fn user_targets_report_missing_failing_and_successful_commands() {
    let sandbox = Sandbox::new();
    let quiet = settings(false, false);
    assert!(matches!(
        run_one(Target::Brew, &quiet, false, None).status,
        Status::Unavailable(reason) if reason.contains("brew not found")
    ));
    sandbox.bin("brew", "case \"$1\" in update) exit 0;; *) echo broken; exit 5;; esac");
    let result = run_one(Target::Brew, &quiet, false, None);
    assert!(matches!(&result.status, Status::Failed(reason) if reason.contains("broken")));
    sandbox.bin("brew", "exit 0");
    let log = file_log(&sandbox.path("brew.log"));
    let result = run_one(Target::Brew, &quiet, false, Some(&log));
    assert!(matches!(result.status, Status::Ok));
    assert!(result.output.is_empty());
    let result = run_one(Target::Brew, &quiet, true, None);
    assert_eq!(result.output.len(), 3);
}

#[test]
fn targets_without_commands_are_skipped_and_pip_is_delegated() {
    let _sandbox = Sandbox::new();
    let quiet = settings(false, false);
    let progress = progress_for(Target::Auto);
    let mut result = TargetResult { target: Target::Auto, status: Status::Ok, output: Vec::new() };
    run_target_inner(Target::Auto, &quiet, false, &mut result, None, &progress.bar(0));
    assert!(matches!(result.status, Status::Skipped(_)));
    assert!(matches!(run_one(Target::Pip, &quiet, false, None).status, Status::Unavailable(_)));
}

fn populate(sandbox: &Sandbox) {
    sandbox.fake_sudo();
    sandbox.fake_system_managers("echo \"$0 $*\"; exit 0");
    for name in ["brew", "flatpak", "uv", "soar", "yay", "choco", "winget", "bun", "pnpm"] {
        sandbox.bin(name, "echo \"$0 $*\"; exit 0");
    }
    sandbox.bin(
        "python3",
        "case \"$*\" in\n\"-m pip --version\") exit 0;;\n\"-m pip list --outdated --format=json\") echo '[{\"name\":\"alpha\"}]';;\n*) echo \"pip $*\";;\nesac",
    );
    set("BUN_INSTALL", sandbox.path("bin"));
    set("PNPM_HOME", sandbox.path("bin"));
}

#[test]
fn dry_runs_cover_every_target() {
    let sandbox = Sandbox::new();
    populate(&sandbox);
    sandbox.run(&["-n", "--disable-log", "all"]).unwrap();
    set("TERMUX_VERSION", "1");
    sandbox.bin("pkg", "exit 0");
    sandbox.bin("apt", "exit 0");
    sandbox.run(&["-n", "pkg"]).unwrap();
    sandbox.run(&["-n", "--silent", "debian"]).unwrap();
}

#[test]
fn real_runs_update_system_and_user_targets() {
    let sandbox = Sandbox::new();
    populate(&sandbox);
    sandbox.run(&["apt", "dnf", "brew", "pip"]).unwrap();
    let logs = fs::read_dir(state_dir().unwrap()).unwrap();
    let text: String = logs
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".log"))
        .map(|entry| fs::read_to_string(entry.path()).unwrap())
        .collect();
    assert!(text.contains("mnn started: targets=apt,brew,dnf,pip mode=sequential"));
    assert!(text.contains("target apt: OK"));
    assert!(text.contains("pip install --upgrade alpha"));
}

#[test]
fn runs_support_every_output_and_scheduling_mode() {
    let sandbox = Sandbox::new();
    populate(&sandbox);
    sandbox.run(&["--parallel", "--silent", "brew", "uv", "flatpak"]).unwrap();
    sandbox.run(&["--parallel", "--verbose", "brew", "uv"]).unwrap();
    sandbox.run(&["-P", "--disable-log", "brew", "uv"]).unwrap();
    sandbox.run(&["--disable-log", "soar"]).unwrap();
    sandbox.config("parallel=true\nsilent=true\n");
    sandbox.run(&["brew", "uv"]).unwrap();
    sandbox.run(&["-v", "-P", "brew", "uv"]).unwrap();
}

#[test]
fn failures_and_unavailable_targets_are_summarised() {
    let sandbox = Sandbox::new();
    populate(&sandbox);
    sandbox.bin("brew", "echo broken; exit 1");
    let error = sandbox
        .run(&["--disable-log", "brew", "winget", "bun", "pnpm", "aur", "--silent"])
        .unwrap_err();
    assert_eq!(error, "one or more package-manager targets failed");
    fs::remove_file(sandbox.path("bin/yay")).unwrap();
    assert_eq!(
        sandbox.run(&["--disable-log", "brew", "aur"]).unwrap_err(),
        "one or more package-manager targets failed"
    );
    sandbox.bin("brew", "exit 0");
    sandbox.run(&["--disable-log", "brew", "aur"]).unwrap();
    fs::remove_file(sandbox.path("bin/uv")).unwrap();
    sandbox.run(&["--disable-log", "uv"]).unwrap();
}

#[test]
fn configuration_and_detection_errors_stop_the_run() {
    let sandbox = Sandbox::new();
    assert_eq!(
        sandbox.run(&["--disable-log"]).unwrap_err(),
        "no supported package managers detected"
    );
    sandbox.config("silent=true\nverbose=true\n");
    assert!(sandbox.run(&["brew"]).unwrap_err().contains("cannot be used together"));
    sandbox.config("nonsense\n");
    assert!(sandbox.run(&["brew"]).unwrap_err().contains("invalid config"));
    sandbox.config("targets=auto\n");
    sandbox.bin("brew", "exit 0");
    sandbox.run(&["--disable-log"]).unwrap();
    sandbox.run(&["--silent", "--disable-log"]).unwrap();
    set("XDG_STATE_HOME", "relative");
    assert!(sandbox.run(&["brew"]).unwrap_err().contains("absolute"));
    unset("XDG_STATE_HOME");
    let state = state_dir().unwrap();
    fs::create_dir_all(&state).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
    let _held = acquire_run_lock().unwrap();
    assert!(
        sandbox.run(&["--disable-log", "brew"]).unwrap_err().contains("another mnn run is active")
    );
    sandbox.fail_console(false);
}

#[test]
fn missing_sudo_stops_system_runs() {
    let sandbox = Sandbox::new();
    sandbox.trusted("apt-get", "exit 0");
    assert!(sandbox.run(&["--disable-log", "apt"]).unwrap_err().contains("no trusted sudo"));
}

#[test]
fn completion_printing_and_installation_run_without_targets() {
    let sandbox = Sandbox::new();
    sandbox.run(&["--print-completion", "bash"]).unwrap();
    sandbox.run(&["--print-completion", "nu"]).unwrap();
    sandbox.run(&["--print-completion", "all"]).unwrap();
    sandbox.run(&["--install-completions", "zsh"]).unwrap();
    sandbox.run(&["--install-completions"]).unwrap();
    assert!(
        sandbox
            .run(&["--install-completions", "bash", "apt"])
            .unwrap_err()
            .contains("cannot be combined with targets")
    );
    assert!(completion_path(CompletionMode::Zsh).unwrap().is_file());
}

#[test]
fn maintenance_actions_are_dispatched() {
    let sandbox = Sandbox::new();
    let source = sandbox.user_file("src/mnn", "binary", 0o700);
    set("MNN_SOURCE_DIR", source.parent().unwrap());
    sandbox.run(&["--install", "--install-completions", "bash"]).unwrap();
    sandbox.run(&["--reinstall"]).unwrap();
    sandbox.run(&["--update"]).unwrap();
    sandbox.run(&["--uninstall"]).unwrap();
    sandbox.run(&["--install"]).unwrap();
    sandbox.run(&["--purge"]).unwrap();
    assert!(!config_path().unwrap().exists());
    sandbox.run(&["--gen-config"]).unwrap();
    assert!(config_path().unwrap().is_file());
    assert!(
        sandbox
            .run(&["--gen-config", "apt"])
            .unwrap_err()
            .contains("cannot be combined with targets")
    );
    assert!(sandbox.run(&["--gen-config"]).unwrap_err().contains("already exists"));
}

#[test]
fn reports_count_and_print_every_status() {
    let sandbox = Sandbox::new();
    let result = |target, status, output: &[&str]| TargetResult {
        target,
        status,
        output: output.iter().map(|line| (*line).to_owned()).collect(),
    };
    let make = || {
        vec![
            result(Target::Apt, Status::Ok, &["line", ""]),
            result(Target::Brew, Status::Unavailable("gone".to_owned()), &[]),
            result(Target::Uv, Status::Skipped("nothing".to_owned()), &[]),
            result(Target::Snap, Status::Failed("broke".to_owned()), &[]),
        ]
    };
    let path = sandbox.path("report.log");
    let log = file_log(&path);
    for (silent, show_progress, log) in [(false, false, Some(&log)), (true, true, None)] {
        assert_eq!(report(make(), silent, show_progress, log).unwrap(), [1, 1, 1, 1]);
    }
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains("target apt: OK"));
    assert!(text.contains("target brew: UNAVAILABLE (gone)"));
    assert!(text.contains("target uv: SKIP (nothing)"));
    assert!(text.contains("target snap: ERR (broke)"));
    assert!(
        report(make(), false, false, Some(&full_log()))
            .unwrap_err()
            .contains("could not write log")
    );
}

#[test]
fn aur_detection_needs_a_helper_next_to_pacman() {
    let sandbox = Sandbox::new();
    sandbox.bin("pacman", "exit 0");
    let targets = detect_targets_for("arch", "");
    assert!(targets.contains(&Target::Pacman) && !targets.contains(&Target::Aur));
    sandbox.bin("trizen", "exit 0");
    assert!(detect_targets_for("arch", "").contains(&Target::Aur));
}
