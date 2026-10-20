fn main() {
    if let Err(error) = shishi_acp::submit::serve_from_args() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
