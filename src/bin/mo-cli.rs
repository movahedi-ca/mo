//! mo-cli entry. Thin wrapper over `mo::cli::run`.

fn main() {
    if let Err(e) = mo::cli::run() {
        eprintln!("mo-cli: {}: {}", e.code, e.message);
        std::process::exit(1);
    }
}
