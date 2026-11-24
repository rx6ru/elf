use elf::config::Paths;
use elf::control::{self, Command};
use std::process::ExitCode;

const USAGE: &str = "usage: elf start | stop | toggle | cancel | paste-again | retry | history [n] | status | quit
       elf press | release      one key for tap (toggle) and hold (push to talk)";

fn main() -> ExitCode {
    let line = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    if Command::parse(&line).is_none() {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    }
    match control::request(&Paths::from_env().socket, &line) {
        Ok(reply) => {
            println!("{reply}");
            if reply.starts_with("ok") { ExitCode::SUCCESS } else { ExitCode::FAILURE }
        }
        Err(e) => {
            eprintln!("elf: cannot reach elfd ({e}). Is it running?");
            ExitCode::FAILURE
        }
    }
}
