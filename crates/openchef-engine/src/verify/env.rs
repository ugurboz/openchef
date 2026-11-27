//! The environment a verification command gets.

use std::ffi::OsString;

/// What a build or test command needs to find its tools and write temporary files.
/// Everything else (API keys, tokens, cloud credentials) stays out: the command runs
/// code an agent just wrote.
const BASE_KEYS: &[&str] = &[
    // Unix
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TMPDIR",
    "LANG",
    "TZ",
    // Windows (case-insensitive there)
    "SYSTEMROOT",
    "SYSTEMDRIVE",
    "COMSPEC",
    "PATHEXT",
    "WINDIR",
    "TEMP",
    "TMP",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMDATA",
    "PROGRAMFILES",
    "PROGRAMFILES(X86)",
    "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE",
];

/// Filters `from` (typically `std::env::vars_os()`) down to the base variables, the
/// locale variables (`LC_*`) and the `extra` keys a project allows. Order is kept, so
/// later entries win. Keys match case-insensitively, so one list works on every OS.
pub fn minimal_env(
    from: impl IntoIterator<Item = (OsString, OsString)>,
    extra: &[&str],
) -> Vec<(OsString, OsString)> {
    let allowed: Vec<String> = BASE_KEYS
        .iter()
        .chain(extra)
        .map(|k| k.to_uppercase())
        .collect();
    from.into_iter()
        .filter(|(k, _)| {
            let up = k.to_string_lossy().to_uppercase();
            !up.is_empty()
                && !up.starts_with('=')
                && (allowed.contains(&up) || up.starts_with("LC_"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::minimal_env;

    #[test]
    fn minimal() {
        let from = [
            ("PATH", "/bin"),
            ("HOME", "/h"),
            ("ANTHROPIC_API_KEY", "sk"),
            ("GITHUB_TOKEN", "t"),
            ("LC_ALL", "C"),
            ("SystemRoot", "C:\\Windows"),
            ("GOFLAGS", "-mod=mod"),
            ("=C:", "C:\\"),
            ("PATH", "/usr/bin"),
        ];
        let got: Vec<String> = minimal_env(
            from.map(|(k, v)| (OsString::from(k), OsString::from(v))),
            &["goflags"],
        )
        .into_iter()
        .map(|(k, v)| format!("{}={}", k.to_string_lossy(), v.to_string_lossy()))
        .collect();
        assert_eq!(
            got,
            [
                "PATH=/bin",
                "HOME=/h",
                "LC_ALL=C",
                "SystemRoot=C:\\Windows",
                "GOFLAGS=-mod=mod",
                "PATH=/usr/bin"
            ]
        );
    }
}
