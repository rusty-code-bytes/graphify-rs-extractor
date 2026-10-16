use clap::{Parser, Subcommand};
use graphify_extractor::{Options, extract};

#[derive(Parser)]
#[command(version, about = "Build a disk-backed structural map for Graphify")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Extract(Options),
}

fn main() {
    let result = match Cli::parse().command {
        Command::Extract(options) => extract(&options),
    };
    match result {
        Ok(stats) => eprintln!("{}", serde_json::to_string_pretty(&stats).unwrap()),
        Err(error) => {
            eprintln!("error: {error:#}");
            std::process::exit(1);
        }
    }
}
