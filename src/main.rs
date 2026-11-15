use clap::{Parser, Subcommand};
use evga_z790_tools::{fan, led};

#[derive(Parser)]
#[command(
    name = "evga",
    version,
    about = "EVGA Z790 CLASSIFIED LED and fan control"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Control ARGB lighting through USB HID
    Led(led::Cli),
    /// Inspect and configure the fan controller (requires root)
    Fan(fan::Cli),
}

fn main() {
    let result = match Cli::parse().command {
        Command::Led(args) => led::run(args),
        Command::Fan(args) => fan::run(args),
    };
    if let Err(error) = result {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}
