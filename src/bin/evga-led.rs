fn main() {
    if let Err(err) = evga_z790_tools::led::run() {
        eprintln!("{err}");
        std::process::exit(1);
    }
}
