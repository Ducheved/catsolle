use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "catsolle", version, about = "catsolle TUI SSH client")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    #[arg(long)]
    pub config: Option<String>,

    #[arg(long)]
    pub project: Option<String>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    Connect {
        target: String,
        #[arg(long)]
        quick: bool,
    },
    Keys {
        #[command(subcommand)]
        command: KeyCommand,
    },
    Config {
        #[arg(long)]
        init: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum KeyCommand {
    Generate {
        #[arg(long)]
        name: String,
        #[arg(long, default_value = "ed25519")]
        algorithm: String,
        #[arg(long)]
        bits: Option<usize>,
        #[arg(long)]
        curve: Option<String>,
    },
    List,
    AddAgent {
        #[arg(long)]
        path: String,
    },
}
