const ENABLED_FEATURES: &[&str] = &[
    #[cfg(feature = "http")]
    "http",
    #[cfg(feature = "http2")]
    "http2",
    #[cfg(feature = "http3")]
    "http3",
    #[cfg(feature = "tls")]
    "tls",
    #[cfg(feature = "websocket")]
    "websocket",
    #[cfg(feature = "webrtc")]
    "webrtc",
];

pub fn enabled_feature_names() -> &'static [&'static str] {
    ENABLED_FEATURES
}

pub fn enabled_feature_list() -> String {
    if ENABLED_FEATURES.is_empty() {
        "none".to_string()
    } else {
        ENABLED_FEATURES.join(", ")
    }
}

pub fn enabled_feature_line() -> String {
    format!("enabled features: {}", enabled_feature_list())
}

pub fn binary_version_text(binary: &str) -> String {
    let git_tag = option_env!("PD_BUILD_GIT_TAG").unwrap_or("untagged");
    let git_commit = option_env!("PD_BUILD_GIT_COMMIT").unwrap_or("unknown");
    let git_dirty = option_env!("PD_BUILD_GIT_DIRTY").unwrap_or("false");
    let dirty = matches!(git_dirty, "true" | "1" | "yes" | "dirty");

    if dirty {
        format!("{binary} {git_tag} (dirty commit: {git_commit})")
    } else if git_commit != "unknown" {
        format!("{binary} {git_tag} (commit: {git_commit})")
    } else {
        format!("{binary} {git_tag}")
    }
}

pub fn binary_version_report(binary: &str) -> String {
    format!(
        "{}\n{}",
        binary_version_text(binary),
        enabled_feature_line()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enabled_feature_names_follow_compile_time_cfg() {
        let names = enabled_feature_names();
        assert_eq!(names.contains(&"http"), cfg!(feature = "http"));
        assert_eq!(names.contains(&"http2"), cfg!(feature = "http2"));
        assert_eq!(names.contains(&"http3"), cfg!(feature = "http3"));
        assert_eq!(names.contains(&"tls"), cfg!(feature = "tls"));
        assert_eq!(names.contains(&"websocket"), cfg!(feature = "websocket"));
        assert_eq!(names.contains(&"webrtc"), cfg!(feature = "webrtc"));
    }

    #[test]
    fn binary_version_report_includes_feature_line() {
        let report = binary_version_report("pd-edge-test");
        assert!(report.contains("pd-edge-test"));
        assert!(report.contains("enabled features: "));
    }
}
