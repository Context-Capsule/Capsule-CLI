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
        _ => context_capsule::local_agent::client::run(arguments),
    }
}
