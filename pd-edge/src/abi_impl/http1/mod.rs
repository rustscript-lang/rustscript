#![cfg_attr(not(feature = "http"), allow(dead_code))]

use axum::http::Version;
use url::Url;

pub(crate) const ALPN_PROTOCOL: &str = "http/1.1";

pub(crate) fn response_version_label(version: Version) -> &'static str {
    match version {
        Version::HTTP_09 => "0.9",
        Version::HTTP_10 => "1.0",
        Version::HTTP_11 => "1.1",
        Version::HTTP_3 => "3",
        _ => "1.1",
    }
}

pub(crate) fn session_origin(target: &str) -> Option<String> {
    let url = Url::parse(target).ok()?;
    let host = url.host_str()?.to_ascii_lowercase();
    let port = url.port_or_known_default()?;
    Some(format!(
        "{}://{}:{}",
        url.scheme().to_ascii_lowercase(),
        host,
        port
    ))
}

#[cfg(test)]
mod tests {
    use axum::http::Version;

    use super::{response_version_label, session_origin};

    #[test]
    fn response_version_label_maps_http1_family_versions() {
        assert_eq!(response_version_label(Version::HTTP_09), "0.9");
        assert_eq!(response_version_label(Version::HTTP_10), "1.0");
        assert_eq!(response_version_label(Version::HTTP_11), "1.1");
        assert_eq!(response_version_label(Version::HTTP_3), "3");
        assert_eq!(response_version_label(Version::HTTP_2), "1.1");
    }

    #[test]
    fn session_origin_normalizes_scheme_host_and_port() {
        assert_eq!(
            session_origin("HTTPS://Example.COM/path?x=1").as_deref(),
            Some("https://example.com:443")
        );
        assert_eq!(
            session_origin("http://Example.COM:8080/path").as_deref(),
            Some("http://example.com:8080")
        );
    }
}
