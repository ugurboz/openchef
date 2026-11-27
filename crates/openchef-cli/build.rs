//! Stamps build metadata into the binary: git commit, commit date, whether the tree
//! was modified, and the compiler version. Values are empty when git is unavailable
//! (e.g. a source tarball); `openchef version` then shows what it knows.

// Build scripts talk to Cargo through stdout.
#![allow(clippy::print_stdout)]

use std::process::Command;

fn output(cmd: &str, args: &[&str]) -> String {
    Command::new(cmd)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map_or_default(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
}

fn main() {
    let commit = output("git", &["rev-parse", "--short=12", "HEAD"]);
    let date = output("git", &["log", "-1", "--format=%cI"]);
    let modified = !output("git", &["status", "--porcelain", "--untracked-files=no"]).is_empty();
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_owned());
    let rustc_version = output(&rustc, &["--version"]);
    println!("cargo:rustc-env=OPENCHEF_COMMIT={commit}");
    println!("cargo:rustc-env=OPENCHEF_COMMIT_DATE={date}");
    println!("cargo:rustc-env=OPENCHEF_MODIFIED={modified}");
    println!("cargo:rustc-env=OPENCHEF_RUSTC={rustc_version}");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/index");
}
