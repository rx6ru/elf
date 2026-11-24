use std::path::Path;
use std::process::ExitCode;

const USAGE: &str = "usage: elfd [--no-hud]                    run the dictation daemon
       elfd --simulate <file.wav> --dry-run   run one dictation from a file; paste nothing";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] => elf::daemon::run(false),
        ["--no-hud"] => elf::daemon::run(true),
        ["--simulate", file, "--dry-run"] | ["--dry-run", "--simulate", file] => elf::simulate::run(Path::new(file)),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("elfd: {e}");
            ExitCode::FAILURE
        }
    }
}
