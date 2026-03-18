#[cfg(test)]
use axum::http::{HeaderMap, HeaderName};
use axum::http::{
    Method,
    header::{CONNECTION, CONTENT_LENGTH, EXPECT, TRANSFER_ENCODING, UPGRADE},
};

use super::state::LazyHttpHeaders;
use super::version::HttpVersionPreference;

pub(crate) const MAX_DOWNSTREAM_HTTP1_FAST_BODY_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DownstreamHttp1FastBodyKind {
    Empty,
    Fixed(usize),
    Chunked,
}

#[cfg(test)]
fn header_contains_token(headers: &HeaderMap, name: HeaderName, token: &str) -> bool {
    headers
        .get_all(name)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .any(|value| value.eq_ignore_ascii_case(token))
}

#[cfg(test)]
fn header_tokens<'a>(
    headers: &'a HeaderMap,
    name: HeaderName,
) -> impl Iterator<Item = &'a str> + 'a {
    headers
        .get_all(name)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
fn parse_content_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
}

#[cfg(test)]
fn request_transfer_encoding_is_chunked_only(headers: &HeaderMap) -> bool {
    let mut saw_chunked = false;
    for token in header_tokens(headers, TRANSFER_ENCODING) {
        if token.eq_ignore_ascii_case("chunked") {
            saw_chunked = true;
        } else {
            return false;
        }
    }
    saw_chunked
}

#[cfg(test)]
pub(crate) fn downstream_http1_fast_path_expects_continue(headers: &HeaderMap) -> bool {
    headers
        .get(EXPECT)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .is_some_and(|value| value.eq_ignore_ascii_case("100-continue"))
}

pub(crate) fn downstream_http1_fast_path_expects_continue_lazy(headers: &LazyHttpHeaders) -> bool {
    headers
        .get_str(EXPECT.as_str())
        .map(|value| value.trim().eq_ignore_ascii_case("100-continue"))
        .unwrap_or(false)
}

#[cfg(test)]
pub(crate) fn classify_downstream_http1_fast_body(
    headers: &HeaderMap,
) -> Option<DownstreamHttp1FastBodyKind> {
    if headers.contains_key(CONTENT_LENGTH) && headers.contains_key(TRANSFER_ENCODING) {
        return None;
    }
    if headers.contains_key(TRANSFER_ENCODING) {
        return request_transfer_encoding_is_chunked_only(headers)
            .then_some(DownstreamHttp1FastBodyKind::Chunked);
    }
    let Some(content_length) = parse_content_length(headers) else {
        return Some(DownstreamHttp1FastBodyKind::Empty);
    };
    if content_length == 0 {
        return Some(DownstreamHttp1FastBodyKind::Empty);
    }
    usize::try_from(content_length)
        .ok()
        .map(DownstreamHttp1FastBodyKind::Fixed)
}

pub(crate) fn classify_downstream_http1_fast_body_lazy(
    headers: &LazyHttpHeaders,
) -> Option<DownstreamHttp1FastBodyKind> {
    if headers.contains_name(CONTENT_LENGTH.as_str())
        && headers.contains_name(TRANSFER_ENCODING.as_str())
    {
        return None;
    }
    if headers.contains_name(TRANSFER_ENCODING.as_str()) {
        return headers
            .header_contains_token(TRANSFER_ENCODING.as_str(), "chunked")
            .then_some(DownstreamHttp1FastBodyKind::Chunked);
    }
    let Some(content_length) = headers.content_length() else {
        return Some(DownstreamHttp1FastBodyKind::Empty);
    };
    if content_length == 0 {
        return Some(DownstreamHttp1FastBodyKind::Empty);
    }
    usize::try_from(content_length)
        .ok()
        .map(DownstreamHttp1FastBodyKind::Fixed)
}

#[cfg(test)]
pub(crate) fn downstream_http1_fast_path_eligible(method: &Method, headers: &HeaderMap) -> bool {
    if method == Method::CONNECT {
        return false;
    }
    if headers.contains_key(EXPECT) && !downstream_http1_fast_path_expects_continue(headers) {
        return false;
    }
    if headers.contains_key(UPGRADE) {
        return false;
    }
    if header_contains_token(headers, CONNECTION, "upgrade") {
        return false;
    }
    classify_downstream_http1_fast_body(headers).is_some()
}

pub(crate) fn downstream_http1_fast_path_eligible_lazy(
    method: &Method,
    headers: &LazyHttpHeaders,
) -> bool {
    if method == Method::CONNECT {
        return false;
    }
    if headers.contains_name(EXPECT.as_str())
        && !downstream_http1_fast_path_expects_continue_lazy(headers)
    {
        return false;
    }
    if headers.contains_name(UPGRADE.as_str()) {
        return false;
    }
    if headers.header_contains_token(CONNECTION.as_str(), "upgrade") {
        return false;
    }
    classify_downstream_http1_fast_body_lazy(headers).is_some()
}

pub(crate) fn outbound_http1_fast_path_eligible(
    version_preference: HttpVersionPreference,
    has_target: bool,
    has_attached_transport: bool,
    has_plain_http1_sender_pool: bool,
    uses_explicit_http2_transport: bool,
    uses_explicit_http3_transport: bool,
) -> bool {
    has_target
        && !has_attached_transport
        && has_plain_http1_sender_pool
        && matches!(
            version_preference,
            HttpVersionPreference::Auto | HttpVersionPreference::Http1
        )
        && !uses_explicit_http2_transport
        && !uses_explicit_http3_transport
}

#[cfg(test)]
mod tests {
    use axum::http::{
        HeaderMap, HeaderValue, Method,
        header::{CONTENT_LENGTH, EXPECT, TRANSFER_ENCODING},
    };

    use super::{
        DownstreamHttp1FastBodyKind, MAX_DOWNSTREAM_HTTP1_FAST_BODY_BYTES,
        classify_downstream_http1_fast_body, downstream_http1_fast_path_eligible,
        downstream_http1_fast_path_expects_continue, outbound_http1_fast_path_eligible,
    };
    use crate::abi_impl::http::version::HttpVersionPreference;

    #[test]
    fn downstream_http1_fast_path_accepts_chunked_and_fixed_requests() {
        let mut fixed = HeaderMap::new();
        fixed.insert(CONTENT_LENGTH, HeaderValue::from_static("16"));
        assert_eq!(
            classify_downstream_http1_fast_body(&fixed),
            Some(DownstreamHttp1FastBodyKind::Fixed(16))
        );
        assert!(downstream_http1_fast_path_eligible(&Method::POST, &fixed));

        let mut chunked = HeaderMap::new();
        chunked.insert(TRANSFER_ENCODING, HeaderValue::from_static("chunked"));
        assert_eq!(
            classify_downstream_http1_fast_body(&chunked),
            Some(DownstreamHttp1FastBodyKind::Chunked)
        );
        assert!(downstream_http1_fast_path_eligible(&Method::POST, &chunked));

        let mut large_fixed = HeaderMap::new();
        large_fixed.insert(
            CONTENT_LENGTH,
            HeaderValue::from_str(&(MAX_DOWNSTREAM_HTTP1_FAST_BODY_BYTES + 1).to_string()).unwrap(),
        );
        assert_eq!(
            classify_downstream_http1_fast_body(&large_fixed),
            Some(DownstreamHttp1FastBodyKind::Fixed(
                MAX_DOWNSTREAM_HTTP1_FAST_BODY_BYTES + 1
            ))
        );
        assert!(downstream_http1_fast_path_eligible(
            &Method::POST,
            &large_fixed
        ));
    }

    #[test]
    fn downstream_http1_fast_path_handles_expect_continue_and_rejects_other_expectations() {
        let mut headers = HeaderMap::new();
        headers.insert(EXPECT, HeaderValue::from_static("100-continue"));
        assert!(downstream_http1_fast_path_expects_continue(&headers));
        assert!(downstream_http1_fast_path_eligible(&Method::POST, &headers));

        headers.insert(EXPECT, HeaderValue::from_static("something-else"));
        assert!(!downstream_http1_fast_path_expects_continue(&headers));
        assert!(!downstream_http1_fast_path_eligible(
            &Method::POST,
            &headers
        ));
    }

    #[test]
    fn downstream_http1_fast_path_rejects_connect() {
        assert!(!downstream_http1_fast_path_eligible(
            &Method::CONNECT,
            &HeaderMap::new()
        ));
    }

    #[test]
    fn outbound_http1_fast_path_requires_http1_no_attached_transport_and_pool() {
        assert!(outbound_http1_fast_path_eligible(
            HttpVersionPreference::Auto,
            true,
            false,
            true,
            false,
            false,
        ));
        assert!(!outbound_http1_fast_path_eligible(
            HttpVersionPreference::Http2,
            true,
            false,
            true,
            false,
            false,
        ));
        assert!(!outbound_http1_fast_path_eligible(
            HttpVersionPreference::Auto,
            true,
            true,
            true,
            false,
            false,
        ));
        assert!(!outbound_http1_fast_path_eligible(
            HttpVersionPreference::Auto,
            true,
            false,
            true,
            true,
            false,
        ));
    }
}
