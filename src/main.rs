use std::ffi::{OsStr, OsString};
use std::io;
use std::path::Path;

mod ego_bridge;
mod framing;
#[cfg(any(target_os = "linux", test))]
mod ipc;
#[cfg(any(target_os = "macos", test))]
mod macos_process;
#[cfg(target_os = "macos")]
mod managed_ssh;

const USAGE: &str = "ego-lite-bridge — headless reverse remote exec bridge for ego-browser\n\nUsage:\n  ego-lite-bridge serve <linux-host>\n  ego-lite-bridge --help\n  ego-lite-bridge --version";

fn main() -> io::Result<()> {
    let args: Vec<OsString> = std::env::args_os().collect();
    if invoked_as_ego_browser(&args) {
        match ego_bridge::run_shim(&args[1..]) {
            Ok(code) => std::process::exit(code),
            Err(err) => {
                eprintln!("ego-browser: {err}");
                std::process::exit(1);
            }
        }
    }

    match args.get(1).map(OsString::as_os_str) {
        Some(command) if command == "serve" && args.len() == 3 => {
            let target = args[2].to_str().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "linux host is not valid UTF-8")
            })?;
            ego_bridge::run_serve(target)
        }
        Some(command) if command == "ego-browser-broker" && args.len() == 2 => {
            ego_bridge::run_broker()
        }
        Some(command) if command == "--help" || command == "-h" => {
            println!("{USAGE}");
            Ok(())
        }
        Some(command) if command == "--version" || command == "-V" => {
            println!("ego-lite-bridge {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    }
}

fn invoked_as_ego_browser(args: &[OsString]) -> bool {
    args.first().and_then(|arg| Path::new(arg).file_name()) == Some(OsStr::new("ego-browser"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_ego_browser_argv_zero() {
        assert!(invoked_as_ego_browser(&[
            "/usr/local/bin/ego-browser".into()
        ]));
        assert!(!invoked_as_ego_browser(&["ego-lite-bridge".into()]));
    }
}
