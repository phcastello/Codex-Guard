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
