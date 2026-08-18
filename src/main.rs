use std::io::{self, Read};

use herdr_mission::{
    cli, process_fixture_request, process_read_only_canary_request,
    process_temporary_fixture_request, BINARY_CONTRACT, PROTOCOL_VERSION,
};
use serde_json::json;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = args.first().map(String::as_str);

    if command == Some("--version") {
        let discovery = json!({
            "binary": "herdr-mission",
            "binary_version": env!("CARGO_PKG_VERSION"),
            "binary_contract": BINARY_CONTRACT,
            "protocol": PROTOCOL_VERSION,
            "operations": ["handle", "drive", "inspect"]
        });
        println!("{discovery}");
        return;
    }

    match command {
        Some("handle") | Some("drive") | Some("inspect") => {
            run_stdin_protocol(command);
        }
        _ => {
            std::process::exit(cli::run(&args));
        }
    }
}

fn run_stdin_protocol(command: Option<&str>) {
    let mut input = String::new();
    if let Err(error) = io::stdin().read_to_string(&mut input) {
        eprintln!("stdin_read_failed: {error}");
        std::process::exit(65);
    }

    let response = match (
        std::env::var_os("HERDR_MISSION_TEMP_ROOT"),
        std::env::var_os("HERDR_MISSION_READ_ONLY_DATABASE"),
    ) {
        (Some(root), None) => process_temporary_fixture_request(command, &input, root.as_ref()),
        (None, Some(database)) => {
            process_read_only_canary_request(command, &input, database.as_ref())
        }
        (None, None) => process_fixture_request(command, &input),
        (Some(_), Some(_)) => process_fixture_request(command, &input),
    };
    println!(
        "{}",
        serde_json::to_string(&response.outcome).expect("kernel outcome must serialize")
    );
    eprintln!("{}", response.diagnostic);
    std::process::exit(response.exit_code);
}
