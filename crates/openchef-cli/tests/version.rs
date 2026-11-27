//! Smoke test: the binary starts and reports its version.

use std::process::Command;

#[test]
fn prints_version() {
    let out = Command::new(env!("CARGO_BIN_EXE_openchef"))
        .output()
        .unwrap();
    assert!(out.status.success(), "exit status {:?}", out.status);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        stdout.trim(),
        format!("openchef {}", env!("CARGO_PKG_VERSION"))
    );
}
