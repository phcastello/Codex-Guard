use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "codex-guard",
    version,
    about = "Interactive, quota-aware frontend for Codex App Server"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
    #[command(flatten)]
    pub run: RunArgs,
}

impl Cli {
    pub fn requires_install(&self) -> bool {
        self.command.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inspection_skips_installation_and_normal_execution_installs() {
        for arguments in [
            vec!["config", "path"],
            vec!["config", "show"],
            vec!["profiles"],
        ] {
            assert!(
                !Cli::try_parse_from(std::iter::once("codex-guard").chain(arguments))
                    .unwrap()
                    .requires_install()
            );
        }
        for argument in ["--help", "--version"] {
            let error = Cli::try_parse_from(["codex-guard", argument]).unwrap_err();
            assert!(matches!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ));
        }
        assert!(Cli::try_parse_from(["codex-guard"])
            .unwrap()
            .requires_install());
        assert!(Cli::try_parse_from([
            "codex-guard",
            "-p",
            "conservative",
            "-c",
            "5",
            "corrija isso"
        ])
        .unwrap()
        .requires_install());
    }
}

#[derive(Subcommand, Debug)]
pub enum Command {
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    Profiles,
}

#[derive(Subcommand, Debug)]
pub enum ConfigCommand {
    Path,
    Show,
}

#[derive(Args, Debug)]
pub struct RunArgs {
    #[arg(short = 'p', long)]
    pub profile: Option<String>,
    #[arg(short = 'c', long)]
    pub credits: Option<f64>,
    #[arg(short = 't', long)]
    pub time: Option<String>,
    #[arg(long)]
    pub attended: bool,
    #[arg(long)]
    pub no_bell: bool,
    #[arg(num_args = 0.., trailing_var_arg = true, value_name = "PROMPT")]
    pub prompt: Vec<String>,
}
