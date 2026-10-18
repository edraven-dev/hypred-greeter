use std::path::PathBuf;

const USAGE: &str = "\
usage: hypred-greeter [options]

  --config <path>   main config file (default /etc/greetd/hypred-greeter/config.toml)
  --layout <path>   layout TOML instead of the config's [paths].layout
  --style <path>    CSS file instead of the config's [paths].style
                    (a relative --layout/--style is taken from the current
                    directory, a relative [paths] value from the config's)
  --demo            windowed demo mode: fake auth; power, button and [keys]
                    commands are announced, not run (label commands do run)
  --check           load config, style and layout as a first start under
                    greetd would (no remembered user) and build the widgets
                    (no window, no greetd, no label commands); print every
                    problem, exit 1 if there is one. Needs a display: run it
                    before logging out onto a new config or package
  --version         print version and exit
  --help            this text";

#[derive(Debug, Default)]
pub struct Args {
    pub config: Option<PathBuf>,
    pub layout: Option<PathBuf>,
    pub style: Option<PathBuf>,
    pub demo: bool,
    pub check: bool,
}

impl Args {
    /// Neither --demo nor --check: greetd is talked to, the state file
    /// written.
    pub fn greetd(&self) -> bool {
        !self.demo && !self.check
    }

    /// The state file is the greeter user's, unreadable to whoever runs
    /// --check: the check judges a first start, the same for everyone.
    pub fn reads_state(&self) -> bool {
        !self.check
    }
}

pub fn parse() -> Args {
    match parse_from(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(why) => {
            eprintln!("{why}\n{USAGE}");
            std::process::exit(1);
        }
    }
}

fn parse_from(argv: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut args = Args::default();
    let mut it = argv.into_iter();
    while let Some(arg) = it.next() {
        let mut path_arg = |name: &str| {
            it.next().map(PathBuf::from).ok_or_else(|| format!("{name} requires a value"))
        };
        match arg.as_str() {
            "--config" => args.config = Some(path_arg("--config")?),
            "--layout" => args.layout = Some(path_arg("--layout")?),
            "--style" => args.style = Some(path_arg("--style")?),
            "--demo" => args.demo = true,
            "--check" => args.check = true,
            "--version" => {
                println!("hypred-greeter {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    if args.check && args.demo {
        return Err("--check loads everything as under greetd: it does not go with --demo".into());
    }
    Ok(args)
}

pub fn check_report(problems: &[String]) -> (Vec<String>, i32) {
    let mut lines: Vec<String> = problems.iter().map(|p| format!("check: {p}")).collect();
    lines.push(match problems.len() {
        0 => "check: no problems".into(),
        1 => "check: 1 problem".into(),
        n => format!("check: {n} problems"),
    });
    (lines, i32::from(!problems.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(argv: &[&str]) -> Result<Args, String> {
        parse_from(argv.iter().map(|arg| arg.to_string()))
    }

    #[test]
    fn check_is_a_flag_of_its_own() {
        let args =
            parse(&["--check", "--config", "/etc/x/config.toml", "--style", "a.css"]).unwrap();
        assert!(args.check && !args.demo);
        assert!(!args.greetd() && !args.reads_state());
        assert_eq!(args.config, Some(PathBuf::from("/etc/x/config.toml")));
        assert_eq!(args.style, Some(PathBuf::from("a.css")));
        assert!(parse(&["--demo"]).is_ok_and(|args| !args.check && !args.greetd()));
        assert!(parse(&["--demo"]).is_ok_and(|args| args.reads_state()));
        assert!(parse(&[]).is_ok_and(|args| args.greetd() && args.reads_state()));
    }

    #[test]
    fn check_does_not_go_with_demo() {
        let err = parse(&["--demo", "--check"]).unwrap_err();
        assert!(err.contains("--demo"), "{err}");
        assert!(parse(&["--check", "--demo"]).is_err());
    }

    #[test]
    fn bad_arguments_are_errors() {
        assert_eq!(parse(&["--layout"]).unwrap_err(), "--layout requires a value");
        assert_eq!(parse(&["--chekc"]).unwrap_err(), "unknown argument: --chekc");
    }

    #[test]
    fn the_check_lists_every_problem_and_fails_on_any() {
        let (lines, code) = check_report(&[]);
        assert_eq!((lines, code), (vec!["check: no problems".to_string()], 0));
        let problems = ["layout has no `password` widget — using built-in layout".to_string()];
        let (lines, code) = check_report(&problems);
        assert_eq!(lines, [format!("check: {}", problems[0]), "check: 1 problem".into()]);
        assert_eq!(code, 1);
        let (lines, code) = check_report(&["a".into(), "b".into()]);
        assert_eq!(lines, ["check: a", "check: b", "check: 2 problems"]);
        assert_eq!(code, 1);
    }
}
