use clap::Parser;

fn main() {
    let cli = magictree::Cli::parse();
    if let Err(err) = magictree::dispatch(cli) {
        eprintln!("magictree: {err:#}");
        std::process::exit(1);
    }
}
