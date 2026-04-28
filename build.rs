use std::{env, process::Command};

const MIN_RUSTC_MAJOR: u32 = 1;
const MIN_RUSTC_MINOR: u32 = 86;

fn main() {
    let rustc = env::var("RUSTC").unwrap_or_else(|_| String::from("rustc"));
    let version_output = match Command::new(&rustc).arg("--version").output() {
        Ok(output) => output,
        Err(error) => {
            println!(
                "cargo:warning=codexia could not determine the active rustc version from `{rustc}`: {error}"
            );
            return;
        }
    };

    let version_text = match String::from_utf8(version_output.stdout) {
        Ok(text) => text,
        Err(error) => {
            println!(
                "cargo:warning=codexia could not decode the active rustc version output as UTF-8: {error}"
            );
            return;
        }
    };

    let (major, minor) = match parse_rustc_version(&version_text) {
        Some(version) => version,
        None => {
            println!(
                "cargo:warning=codexia could not parse the active rustc version from `{}`",
                version_text.trim()
            );
            return;
        }
    };

    if (major, minor) < (MIN_RUSTC_MAJOR, MIN_RUSTC_MINOR) {
        panic!(
            "codexia requires rustc {}.{} or newer, but `{}` is active. \
Update Rust with `rustup update stable` or install a newer toolchain, then retry.",
            MIN_RUSTC_MAJOR,
            MIN_RUSTC_MINOR,
            version_text.trim()
        );
    }
}

fn parse_rustc_version(version_text: &str) -> Option<(u32, u32)> {
    let mut parts = version_text.split_whitespace();
    if parts.next()? != "rustc" {
        return None;
    }

    let version = parts.next()?;
    let mut numbers = version.split('.');
    let major = numbers.next()?.parse().ok()?;
    let minor = numbers.next()?.parse().ok()?;
    Some((major, minor))
}
