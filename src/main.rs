fn main() {
    let cli = magictree::parse();
    if let Err(err) = magictree::dispatch(cli) {
        eprintln!("magictree: {err:#}");
        std::process::exit(1);
    }
}
