fn main() -> std::process::ExitCode {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    match arguments.first().map(String::as_str) {
        Some("--agent-serve") if arguments.len() == 1 => {
            match context_capsule::local_agent::server::serve() {
                Ok(()) => std::process::ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("Context Capsule Local Agent failed: {error}");
                    std::process::ExitCode::from(1)
                }
            }
        }
        // Desktop reads are intentionally direct and side-effect free. Mutating
        // save/restore/service operations continue through the Local Agent and
        // the mature CLI transaction paths below.
        Some("desktop") => context_capsule::desktop_api::run(arguments[1..].to_vec()),
        _ => context_capsule::local_agent::client::run(arguments),
    }
}
