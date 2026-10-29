//! The version string reported in serverInfo: what is running, not just the crate version.

/// Semantic version from Cargo.toml.
const PACKAGE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Formats `<version> (Rust <rustc>, commit <sha>, built <date>)`. A missing or empty
/// commit or date falls back to `dev` and `unknown`, as in local builds without the deploy script.
fn format_server_version(
    version: &str,
    rustc: &str,
    commit: Option<&str>,
    built: Option<&str>,
) -> String {
    let commit = non_empty(commit).unwrap_or("dev");
    let built = non_empty(built).unwrap_or("unknown");
    format!("{version} (Rust {rustc}, commit {commit}, built {built})")
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.is_empty())
}

/// The version reported in serverInfo for this build.
pub fn server_version() -> String {
    format_server_version(
        PACKAGE_VERSION,
        env!("HEVY_RUSTC_VERSION"),
        option_env!("GIT_SHA"),
        option_env!("BUILD_DATE"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_version_rustc_commit_and_build_date() {
        assert_eq!(
            format_server_version("0.1.0", "1.97.0", Some("1a012fd"), Some("2026-10-29")),
            "0.1.0 (Rust 1.97.0, commit 1a012fd, built 2026-10-29)",
        );
    }

    #[test]
    fn falls_back_to_dev_and_unknown_without_build_args() {
        assert_eq!(
            format_server_version("0.1.0", "1.97.0", None, None),
            "0.1.0 (Rust 1.97.0, commit dev, built unknown)",
        );
    }

    #[test]
    fn falls_back_to_dev_and_unknown_for_empty_build_args() {
        assert_eq!(
            format_server_version("0.1.0", "1.97.0", Some(""), Some("")),
            "0.1.0 (Rust 1.97.0, commit dev, built unknown)",
        );
    }

    #[test]
    fn server_version_names_package_and_toolchain() {
        let version = server_version();
        assert!(
            version.starts_with(&format!("{PACKAGE_VERSION} (Rust ")),
            "{version}",
        );
        assert!(version.contains(", commit "), "{version}");
        assert!(version.contains(", built "), "{version}");
        assert!(version.ends_with(')'), "{version}");
    }
}
