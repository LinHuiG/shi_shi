fn main() {
    let ticket =
        std::env::args().skip_while(|a| a != "--analysis-submit-mcp").nth(1).expect("--analysis-submit-mcp <ticket>");
    if let Err(error) = shishi_acp::submit::serve(std::path::Path::new(&ticket)) {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
