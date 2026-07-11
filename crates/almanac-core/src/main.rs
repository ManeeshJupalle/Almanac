use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--self-check") => match almanac_core::self_check() {
            Ok(path) => {
                eprintln!("db: {}", path.display());
                println!("core ok");
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("self-check failed: {err:#}");
                ExitCode::FAILURE
            }
        },
        _ => {
            eprintln!("usage: almanac-core --self-check");
            ExitCode::from(2)
        }
    }
}
