//! The environment of a confined process (§21.3, D-032): a private `TMPDIR` always,
//! and tool caches redirected to a per-repository cache, so builds work without
//! writing (or poisoning) the user's global caches.

use std::path::Path;

/// The cache variables set, with their directory under the repository's cache.
pub const CACHE_VARS: [(&str, &str); 6] = [
    ("GOCACHE", "go/build"),
    ("GOMODCACHE", "go/mod"),
    ("GOPATH", "go/path"),
    ("npm_config_cache", "npm"),
    ("PIP_CACHE_DIR", "pip"),
    ("CARGO_HOME", "cargo"),
];

/// The variables to set on a confined process: `TMPDIR` (and `TMP`/`TEMP`) to `temp`,
/// tool caches under `cache`. Both directories must be writable in the policy.
#[must_use]
pub fn sandbox_env(temp: &Path, cache: &Path) -> Vec<(String, String)> {
    let temp = temp.to_string_lossy().into_owned();
    let mut out = vec![
        ("TMPDIR".to_owned(), temp.clone()),
        ("TMP".to_owned(), temp.clone()),
        ("TEMP".to_owned(), temp),
    ];
    out.extend(CACHE_VARS.iter().map(|(var, rel)| {
        (
            (*var).to_owned(),
            cache.join(rel).to_string_lossy().into_owned(),
        )
    }));
    out
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::sandbox_env;

    #[test]
    fn private_temp_and_caches() {
        let env = sandbox_env(Path::new("/t"), Path::new("/c"));
        let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("TMPDIR"), Some("/t"));
        assert_eq!(get("GOCACHE"), Some("/c/go/build"));
        assert_eq!(get("npm_config_cache"), Some("/c/npm"));
        assert_eq!(get("CARGO_HOME"), Some("/c/cargo"));
    }
}
