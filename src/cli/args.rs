//! Reading the command line: a command, its positional arguments, and options
//! before, between or after them.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Ls,
    Upload,
    Download,
    Delete,
    Purge,
    Connections,
    Update,
}

impl Command {
    pub const ALL: [Command; 7] = [
        Command::Ls,
        Command::Upload,
        Command::Download,
        Command::Delete,
        Command::Purge,
        Command::Connections,
        Command::Update,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Command::Ls => "ls",
            Command::Upload => "upload",
            Command::Download => "download",
            Command::Delete => "delete",
            Command::Purge => "purge",
            Command::Connections => "connections",
            Command::Update => "update",
        }
    }

    fn named(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.name() == name)
    }

    /// The options this command takes, besides --help.
    fn options(self) -> &'static [&'static str] {
        match self {
            Command::Ls => &["connection", "regex", "long"],
            Command::Upload => &["connection", "regex", "dry-run", "to", "purge"],
            Command::Download => &["connection", "regex", "dry-run", "to"],
            Command::Delete => &["connection", "regex", "dry-run", "yes", "purge"],
            Command::Purge => &["connection", "regex", "dry-run"],
            Command::Connections => &[],
            Command::Update => &["check"],
        }
    }
}

struct Spec {
    long: &'static str,
    short: Option<char>,
    takes_value: bool,
}

const OPTIONS: [Spec; 11] = [
    Spec {
        long: "connection",
        short: Some('c'),
        takes_value: true,
    },
    Spec {
        long: "regex",
        short: Some('E'),
        takes_value: false,
    },
    Spec {
        long: "dry-run",
        short: Some('n'),
        takes_value: false,
    },
    Spec {
        long: "yes",
        short: Some('y'),
        takes_value: false,
    },
    Spec {
        long: "to",
        short: None,
        takes_value: true,
    },
    Spec {
        long: "purge",
        short: None,
        takes_value: false,
    },
    Spec {
        long: "long",
        short: Some('l'),
        takes_value: false,
    },
    Spec {
        long: "check",
        short: None,
        takes_value: false,
    },
    Spec {
        long: "help",
        short: Some('h'),
        takes_value: false,
    },
    Spec {
        long: "version",
        short: Some('V'),
        takes_value: false,
    },
    // Lets `fth -?` show help, as many Windows tools do.
    Spec {
        long: "help",
        short: Some('?'),
        takes_value: false,
    },
];

#[derive(Debug)]
pub struct Args {
    pub command: Command,
    pub positional: Vec<String>,
    pub connection: Option<String>,
    pub regex: bool,
    pub dry_run: bool,
    pub yes: bool,
    pub to: Option<String>,
    pub purge: bool,
    pub long: bool,
    pub check: bool,
}

#[derive(Debug)]
pub enum Parsed {
    Run(Args),
    /// Help for one command, or for the tool.
    Help(Option<Command>),
    Version,
}

/// A command line that can't be run, and the command it was for.
#[derive(Debug)]
pub struct Mistake {
    pub message: String,
    pub command: Option<Command>,
}

pub fn parse(arguments: &[String]) -> Result<Parsed, Mistake> {
    let mut given: Vec<(&'static str, Option<String>)> = Vec::new();
    let mut positional: Vec<String> = Vec::new();
    let mut rest = arguments.iter();
    let mut options_done = false;
    let fail = |message: String| Mistake {
        message,
        command: None,
    };

    while let Some(argument) = rest.next() {
        if options_done || argument == "-" || !argument.starts_with('-') {
            positional.push(argument.clone());
        } else if argument == "--" {
            options_done = true;
        } else if let Some(long) = argument.strip_prefix("--") {
            let (name, inline) = match long.split_once('=') {
                Some((name, value)) => (name, Some(value.to_string())),
                None => (long, None),
            };
            let spec = OPTIONS
                .iter()
                .find(|s| s.long == name)
                .ok_or_else(|| fail(format!("There's no option --{name}")))?;
            let value = match (spec.takes_value, inline) {
                (true, Some(value)) => Some(value),
                (true, None) => Some(
                    rest.next()
                        .cloned()
                        .ok_or_else(|| fail(format!("--{name} needs a value")))?,
                ),
                (false, Some(_)) => return Err(fail(format!("--{name} doesn't take a value"))),
                (false, None) => None,
            };
            given.push((spec.long, value));
        } else {
            // Short options, which can be combined: `-ny`, `-c work`, `-cwork`.
            let letters: Vec<char> = argument[1..].chars().collect();
            for (index, &letter) in letters.iter().enumerate() {
                let spec = OPTIONS
                    .iter()
                    .find(|s| s.short == Some(letter))
                    .ok_or_else(|| fail(format!("There's no option -{letter}")))?;
                if !spec.takes_value {
                    given.push((spec.long, None));
                    continue;
                }
                let attached: String = letters[index + 1..].iter().collect();
                let value = if attached.is_empty() {
                    rest.next()
                        .cloned()
                        .ok_or_else(|| fail(format!("-{letter} needs a value")))?
                } else {
                    attached
                };
                given.push((spec.long, Some(value)));
                break;
            }
        }
    }

    let has = |name: &str| given.iter().any(|(n, _)| *n == name);
    if positional.is_empty() {
        return Ok(if has("version") && !has("help") {
            Parsed::Version
        } else {
            Parsed::Help(None)
        });
    }
    let name = positional.remove(0);
    if name == "help" {
        let topic = match positional.first() {
            Some(topic) => Some(
                Command::named(topic).ok_or_else(|| fail(format!("There's no command {topic}")))?,
            ),
            None => None,
        };
        return Ok(Parsed::Help(topic));
    }
    let command =
        Command::named(&name).ok_or_else(|| fail(format!("There's no command {name}")))?;
    if has("help") {
        return Ok(Parsed::Help(Some(command)));
    }

    let mut args = Args {
        command,
        positional,
        connection: None,
        regex: false,
        dry_run: false,
        yes: false,
        to: None,
        purge: false,
        long: false,
        check: false,
    };
    for (name, value) in given {
        if name == "version" {
            continue;
        }
        if !command.options().contains(&name) {
            return Err(Mistake {
                message: format!("{} doesn't take --{name}", command.name()),
                command: Some(command),
            });
        }
        match name {
            "connection" => args.connection = value,
            "regex" => args.regex = true,
            "dry-run" => args.dry_run = true,
            "yes" => args.yes = true,
            "to" => args.to = value,
            "purge" => args.purge = true,
            "long" => args.long = true,
            "check" => args.check = true,
            _ => {}
        }
    }
    Ok(Parsed::Run(args))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_line(line: &str) -> Result<Parsed, Mistake> {
        let arguments: Vec<String> = line.split_whitespace().map(String::from).collect();
        parse(&arguments)
    }

    fn run(line: &str) -> Args {
        match parse_line(line) {
            Ok(Parsed::Run(args)) => args,
            other => panic!("{line}: {other:?}"),
        }
    }

    fn mistake(line: &str) -> String {
        match parse_line(line) {
            Err(mistake) => mistake.message,
            other => panic!("{line}: {other:?}"),
        }
    }

    #[test]
    fn reads_options_anywhere() {
        let args = run("-c work delete media photos/ --dry-run -y");
        assert_eq!(args.command, Command::Delete);
        assert_eq!(args.positional, ["media", "photos/"]);
        assert_eq!(args.connection.as_deref(), Some("work"));
        assert!(args.dry_run && args.yes && !args.regex);

        let args = run("download media x --to=out -E");
        assert_eq!(args.to.as_deref(), Some("out"));
        assert!(args.regex);

        let args = run("delete media -Eny -cwork");
        assert!(args.regex && args.dry_run && args.yes);
        assert_eq!(args.connection.as_deref(), Some("work"));
    }

    #[test]
    fn a_double_dash_ends_the_options() {
        let args = run("delete media -- -odd-name --yes");
        assert_eq!(args.positional, ["media", "-odd-name", "--yes"]);
        assert!(!args.yes);
    }

    #[test]
    fn asks_for_help_and_the_version() {
        assert!(matches!(parse_line(""), Ok(Parsed::Help(None))));
        assert!(matches!(parse_line("--help"), Ok(Parsed::Help(None))));
        assert!(matches!(parse_line("-V"), Ok(Parsed::Version)));
        assert!(matches!(
            parse_line("help upload"),
            Ok(Parsed::Help(Some(Command::Upload)))
        ));
        assert!(matches!(
            parse_line("purge -h"),
            Ok(Parsed::Help(Some(Command::Purge)))
        ));
    }

    #[test]
    fn rejects_what_a_command_doesnt_take() {
        assert_eq!(mistake("frobnicate"), "There's no command frobnicate");
        assert_eq!(mistake("ls --colour"), "There's no option --colour");
        assert_eq!(mistake("ls -z"), "There's no option -z");
        assert_eq!(
            mistake("download media x --purge"),
            "download doesn't take --purge"
        );
        assert_eq!(mistake("ls media --yes"), "ls doesn't take --yes");
        assert_eq!(mistake("upload media x --to"), "--to needs a value");
        assert_eq!(mistake("ls --long=yes"), "--long doesn't take a value");
    }
}
