//! `fth`, the command line tool: lists, uploads, downloads, deletes
//! and purges from the cache the objects that paths, globs or regular
//! expressions pick, using the connections saved in the app.

mod args;
mod commands;
mod help;

use std::fmt::Display;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::config::Store;
use crate::system;
use crate::update::{self, Checks, GitHub, Notice};

use args::{Command, Parsed};

/// What a run of the tool works with. [`main`] fills it in from the process;
/// tests substitute their own.
pub struct Context<'a> {
    pub store: &'a Store,
    /// Reads an environment variable; empty values count as unset.
    pub env: &'a dyn Fn(&str) -> Option<String>,
    /// Where relative local paths start.
    pub cwd: PathBuf,
    pub out: &'a mut dyn Write,
    pub err: &'a mut dyn Write,
    pub input: &'a mut dyn BufRead,
    /// Whether someone is there to answer a question.
    pub interactive: bool,
    /// Whether to keep a progress line on `err`, which must be a terminal.
    pub progress: bool,
    /// The settings folder, where checks for updates are remembered. When
    /// set, commands mention a newer release, checking at most once a day.
    pub updates: Option<PathBuf>,
}

/// Runs the tool with the process's arguments, settings and terminal, and
/// returns the exit code.
pub fn main() -> i32 {
    // Arguments that aren't valid Unicode can't name keys anyway.
    let arguments: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let config_dir = system::config_dir();
    if let Some(old) = system::old_config_dir() {
        system::move_old_settings(&old, &config_dir);
    }
    update::tidy();
    let store = Store::load(config_dir.clone());
    let env = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
    let stdin = io::stdin();
    let terminal = io::stderr().is_terminal();
    let mut cx = Context {
        store: &store,
        env: &env,
        cwd: std::env::current_dir().unwrap_or_default(),
        out: &mut io::stdout().lock(),
        err: &mut io::stderr().lock(),
        interactive: terminal && stdin.is_terminal(),
        input: &mut stdin.lock(),
        progress: terminal,
        // Scripts and CI don't need to hear about updates.
        updates: (terminal && update::checks_on_its_own()).then_some(config_dir),
    };
    run(&arguments, &mut cx)
}

/// Runs one command line. The exit code is 0 when everything worked, 1 when
/// something failed and 2 when the command line was wrong.
pub fn run(arguments: &[String], cx: &mut Context) -> i32 {
    let mut cli = Cli { cx, status: 0 };
    let args = match args::parse(arguments) {
        Ok(Parsed::Run(args)) => args,
        Ok(Parsed::Help(topic)) => {
            cli.say(topic.map_or(help::OVERVIEW, help::command).trim_end());
            return 0;
        }
        Ok(Parsed::Version) => {
            cli.say(concat!("fth ", env!("CARGO_PKG_VERSION")));
            return 0;
        }
        Err(mistake) => return cli.usage(&mistake.message, mistake.command),
    };
    let reminder = match &cli.cx.updates {
        Some(settings) if args.command != Command::Update => {
            Some(Reminder::start(settings, cli.env("FATHOM_UPDATE_API")))
        }
        _ => None,
    };
    let code = match commands::run(&args, &mut cli) {
        Ok(()) => 0,
        Err(Error::Usage(message)) => cli.usage(&message, Some(args.command)),
        Err(Error::Failed(message)) => {
            cli.warn(message);
            1
        }
        Err(Error::Reported) => 1,
    };
    if let Some(notice) = reminder.and_then(Reminder::finish) {
        cli.clear_status();
        let _ = writeln!(
            cli.cx.err,
            "\n{} Run fth update to install it.",
            available(&notice.version)
        );
    }
    code
}

/// `Fathom 0.0.5 is available. You have 0.0.4.`, as the app says it too.
fn available(version: &str) -> String {
    format!(
        "Fathom {version} is available. You have {}.",
        update::current()
    )
}

/// How long a command waits, at most, for a check that started with it.
const REMINDER_WAIT: Duration = Duration::from_millis(1500);

/// A check for updates that runs alongside a command, to mention a newer
/// release once the command is done.
struct Reminder {
    known: Option<Notice>,
    /// Gets the newer release, if any, or nothing when the check failed.
    checking: Option<mpsc::Receiver<Option<Option<Notice>>>>,
    started: Instant,
}

impl Reminder {
    fn start(settings: &Path, api: Option<String>) -> Self {
        let checks = Checks::new(settings);
        let mut reminder = Self {
            known: checks.known(),
            checking: None,
            started: Instant::now(),
        };
        if checks.due() {
            checks.postpone();
            let (sender, receiver) = mpsc::channel();
            std::thread::spawn(move || {
                let found = GitHub::new(api.as_deref()).latest().ok().map(|release| {
                    checks.record(&release);
                    release.is_newer().then(|| Notice::from(&release))
                });
                let _ = sender.send(found);
            });
            reminder.checking = Some(receiver);
        }
        reminder
    }

    /// The newer release, if there is one. A check still running gets a
    /// moment to finish; one that takes longer is left for next time.
    fn finish(self) -> Option<Notice> {
        match self.checking {
            None => self.known,
            Some(receiver) => {
                let left = REMINDER_WAIT.saturating_sub(self.started.elapsed());
                match receiver.recv_timeout(left) {
                    Ok(Some(fresh)) => fresh,
                    _ => self.known,
                }
            }
        }
    }
}

enum Error {
    /// The command line asks for something that can't be done.
    Usage(String),
    Failed(String),
    /// Something failed, and has been reported already.
    Reported,
}

impl From<String> for Error {
    fn from(message: String) -> Self {
        Error::Failed(message)
    }
}

impl From<crate::s3::Error> for Error {
    fn from(error: crate::s3::Error) -> Self {
        Error::Failed(error.to_string())
    }
}

/// A run in progress: the context, and the progress line on screen.
struct Cli<'a, 'b> {
    cx: &'a mut Context<'b>,
    /// How many characters the progress line takes up, so it can be cleared.
    status: usize,
}

impl Cli<'_, '_> {
    /// Writes a line of results.
    fn say(&mut self, text: impl Display) {
        self.clear_status();
        let _ = writeln!(self.cx.out, "{text}");
    }

    /// Writes a problem to the error stream.
    fn warn(&mut self, text: impl Display) {
        self.clear_status();
        let _ = writeln!(self.cx.err, "fth: {text}");
    }

    fn usage(&mut self, message: &str, command: Option<Command>) -> i32 {
        let help = match command {
            Some(command) => format!("fth help {}", command.name()),
            None => "fth help".to_string(),
        };
        self.warn(format!("{message}. Run {help} for usage."));
        2
    }

    /// Replaces the progress line, when there is a terminal to show it on.
    fn status(&mut self, text: &str) {
        if !self.cx.progress {
            return;
        }
        let width = text.chars().count();
        let padding = " ".repeat(self.status.saturating_sub(width));
        let _ = write!(self.cx.err, "\r{text}{padding}");
        let _ = self.cx.err.flush();
        self.status = width;
    }

    fn clear_status(&mut self) {
        if self.status > 0 {
            let _ = write!(self.cx.err, "\r{}\r", " ".repeat(self.status));
            let _ = self.cx.err.flush();
            self.status = 0;
        }
    }

    /// Asks a yes or no question. No one to answer, or no answer, is a no.
    fn confirm(&mut self, question: &str) -> bool {
        if !self.cx.interactive {
            return false;
        }
        self.clear_status();
        let _ = write!(self.cx.err, "{question} [y/N] ");
        let _ = self.cx.err.flush();
        let mut answer = String::new();
        if self.cx.input.read_line(&mut answer).is_err() {
            return false;
        }
        matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
    }

    fn env(&self, name: &str) -> Option<String> {
        (self.cx.env)(name)
    }

    /// A local path as it is best shown: relative to the current folder when
    /// it is inside it.
    fn shown(&self, path: &Path) -> String {
        match path.strip_prefix(&self.cx.cwd) {
            Ok(relative) if relative.as_os_str().is_empty() => ".".to_string(),
            Ok(relative) => relative.display().to_string(),
            Err(_) => path.display().to_string(),
        }
    }
}
