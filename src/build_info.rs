//! Compile-time release channel and build identity.

/// Release channel baked into this executable (`dev`, `preview`, or `stable`).
pub const BUILD_CHANNEL: &str = match option_env!("DBOTTER_BUILD_CHANNEL") {
    Some(channel) => channel,
    None => "dev",
};

/// Immutable build identifier baked into this executable.
pub const BUILD_ID: &str = match option_env!("DBOTTER_BUILD_ID") {
    Some(id) => id,
    None => "dev",
};

/// Package version from the root `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Version text clap appends after the binary name for `--version`.
pub fn version_with_build() -> String {
    format_version(VERSION, BUILD_CHANNEL, BUILD_ID)
}

fn format_version(version: &str, channel: &str, build_id: &str) -> String {
    format!("{version} ({channel} {build_id})")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_preview_identity() {
        assert_eq!(
            format_version("0.1.0", "preview", "2026-07-14-0905-0123456789ab"),
            "0.1.0 (preview 2026-07-14-0905-0123456789ab)"
        );
    }

    #[test]
    fn local_build_has_explicit_identity() {
        assert!(!VERSION.is_empty());
        assert!(!BUILD_CHANNEL.is_empty());
        assert!(!BUILD_ID.is_empty());
        assert_eq!(
            version_with_build(),
            format_version(VERSION, BUILD_CHANNEL, BUILD_ID)
        );
    }
}
