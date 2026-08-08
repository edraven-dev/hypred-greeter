//! Command-line arguments. Six flags don't justify a parser dependency.

use std::path::PathBuf;

const USAGE: &str = "\
usage: hypred-greeter [options]

  --config <path>   main config file (default /etc/greetd/hypred-greeter/config.toml)
  --layout <path>   layout TOML, overrides the config's [paths].layout
  --style <path>    CSS file, overrides the config's [paths].style
  --demo            windowed demo mode: fake auth, nothing is executed
  --version         print version and exit
  --help            this text";

#[derive(Debug, Default)]
pub struct Args {
    pub config: Option<PathBuf>,
    pub layout: Option<PathBuf>,
    pub style: Option<PathBuf>,
    pub demo: bool,
}

pub fn parse() -> Args {
    let mut args = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut path_arg = |name: &str| match it.next() {
            Some(v) => PathBuf::from(v),
            None => {
                eprintln!("{name} requires a value\n{USAGE}");
                std::process::exit(1);
            }
        };
        match arg.as_str() {
            "--config" => args.config = Some(path_arg("--config")),
            "--layout" => args.layout = Some(path_arg("--layout")),
            "--style" => args.style = Some(path_arg("--style")),
            "--demo" => args.demo = true,
            "--version" => {
                println!("hypred-greeter {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown argument: {other}\n{USAGE}");
                std::process::exit(1);
            }
        }
    }
    args
}
