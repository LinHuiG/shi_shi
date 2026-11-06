//! Synthetic command for the tool runner tests, the same on `sh` and
//! `cmd.exe`. Arguments are steps run in order:
//! `out <text>`, `err <text>`, `fill <n>`, `invalid`, `touch <path>`,
//! `sleep <seconds>`, `child <seconds>` (a sleeping child in the same tree)
//! and `exit <code>`.

use std::io::Write;
use std::time::Duration;

fn main() {
    let mut args = std::env::args().skip(1);
    while let Some(step) = args.next() {
        let mut next = || args.next().expect("step argument");
        match step.as_str() {
            "out" => print!("{}", next()),
            "err" => eprint!("{}", next()),
            "fill" => print!("{}", "a".repeat(next().parse().unwrap())),
            "invalid" => std::io::stdout().write_all(&[0xff]).unwrap(),
            "touch" => drop(std::fs::File::create(next()).unwrap()),
            "sleep" => {
                std::io::stdout().flush().unwrap();
                std::thread::sleep(Duration::from_secs_f64(next().parse().unwrap()));
            }
            "child" => {
                let exe = std::env::current_exe().unwrap();
                std::process::Command::new(exe).args(["sleep", &next()]).spawn().unwrap();
            }
            "exit" => {
                std::io::stdout().flush().unwrap();
                std::process::exit(next().parse().unwrap());
            }
            other => panic!("unknown step {other}"),
        }
    }
}
