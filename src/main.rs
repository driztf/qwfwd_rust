mod ban;
mod cmd;
mod console;
mod cvar;
mod fs;
mod huff;
mod info;
mod msg;
mod net;
mod pacer;
mod parse;
mod peer;
mod protocol;
mod proxy;
mod query;
mod stats;
mod timer;

mod whitelist;

use std::process::ExitCode;

use proxy::Params;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().collect();

    if let Some(first) = argv.get(1)
        && matches!(
            first.to_ascii_lowercase().as_str(),
            "-h" | "-?" | "/?" | "/h" | "/help" | "-help" | "--help"
        )
    {
        cprint!("Usage: {} [port [ip]]\n", argv[0]);
        return ExitCode::FAILURE;
    }

    let port = match argv.get(1).filter(|s| !s.starts_with(['-', '+'])) {
        None => None,
        Some(arg) => match arg.parse::<u16>() {
            Ok(0) => None,
            Ok(port) => Some(port),
            Err(_) => {
                cprint!("invalid port: {arg}\n");
                return ExitCode::FAILURE;
            }
        },
    };
    let ip = argv
        .get(2)
        .filter(|s| !s.starts_with(['-', '+']))
        .cloned()
        .unwrap_or_default();

    match proxy::run(Params { port, ip, argv }).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            cprint!("{err}\n");
            ExitCode::FAILURE
        }
    }
}
