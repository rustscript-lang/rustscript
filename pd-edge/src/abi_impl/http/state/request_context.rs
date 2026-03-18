use super::*;

static NEXT_HTTP_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Default)]
pub(crate) struct LazyRequestId {
    value: Arc<OnceLock<String>>,
}

impl LazyRequestId {
    pub(crate) fn deferred() -> Self {
        Self::default()
    }

    pub(crate) fn from_string(value: String) -> Self {
        let stored = OnceLock::new();
        let _ = stored.set(value);
        Self {
            value: Arc::new(stored),
        }
    }

    pub(crate) fn as_str(&self) -> &str {
        self.value.get_or_init(|| {
            format!(
                "req-{:016x}",
                NEXT_HTTP_REQUEST_ID.fetch_add(1, Ordering::Relaxed)
            )
        })
    }
}

#[derive(Debug)]
pub(crate) struct DownstreamDerivedRequestParts {
    uri: Uri,
    version: Version,
    headers: LazyHttpHeaders,
    connection_metadata: Option<DownstreamConnectionMetadata>,
    path: OnceLock<String>,
    query: OnceLock<String>,
    http_version: OnceLock<String>,
    scheme: OnceLock<String>,
    host: OnceLock<String>,
    client_ip: OnceLock<String>,
    port: OnceLock<u16>,
}

impl DownstreamDerivedRequestParts {
    fn new(
        uri: Uri,
        version: Version,
        headers: LazyHttpHeaders,
        connection_metadata: Option<DownstreamConnectionMetadata>,
    ) -> Self {
        Self {
            uri,
            version,
            headers,
            connection_metadata,
            path: OnceLock::new(),
            query: OnceLock::new(),
            http_version: OnceLock::new(),
            scheme: OnceLock::new(),
            host: OnceLock::new(),
            client_ip: OnceLock::new(),
            port: OnceLock::new(),
        }
    }

    fn path(&self) -> &str {
        self.path.get_or_init(|| self.uri.path().to_string())
    }

    fn query(&self) -> &str {
        self.query
            .get_or_init(|| self.uri.query().unwrap_or("").to_string())
    }

    fn http_version(&self) -> &str {
        self.http_version
            .get_or_init(|| http_version_label(self.version).to_string())
    }

    fn scheme(&self) -> &str {
        self.scheme.get_or_init(|| {
            resolve_downstream_request_scheme(
                &self.uri,
                &self.headers,
                self.connection_metadata.as_ref(),
            )
        })
    }

    fn host(&self) -> &str {
        self.host
            .get_or_init(|| resolve_downstream_request_host(&self.uri, &self.headers))
    }

    fn client_ip(&self) -> &str {
        self.client_ip.get_or_init(|| {
            resolve_downstream_request_client_ip(&self.headers, self.connection_metadata.as_ref())
        })
    }

    fn port(&self) -> u16 {
        *self.port.get_or_init(|| {
            resolve_downstream_request_port(
                &self.uri,
                &self.headers,
                self.scheme(),
                self.connection_metadata.as_ref(),
            )
        })
    }
}

impl Clone for DownstreamDerivedRequestParts {
    fn clone(&self) -> Self {
        fn clone_once_lock<T: Clone>(source: &OnceLock<T>) -> OnceLock<T> {
            let cloned = OnceLock::new();
            if let Some(value) = source.get() {
                let _ = cloned.set(value.clone());
            }
            cloned
        }

        Self {
            uri: self.uri.clone(),
            version: self.version,
            headers: self.headers.clone(),
            connection_metadata: self.connection_metadata.clone(),
            path: clone_once_lock(&self.path),
            query: clone_once_lock(&self.query),
            http_version: clone_once_lock(&self.http_version),
            scheme: clone_once_lock(&self.scheme),
            host: clone_once_lock(&self.host),
            client_ip: clone_once_lock(&self.client_ip),
            port: clone_once_lock(&self.port),
        }
    }
}

#[derive(Debug)]
pub(crate) struct DeferredDownstreamDerivedRequestParts {
    uri: Uri,
    version: Version,
    headers: LazyHttpHeaders,
    connection_metadata: Option<DownstreamConnectionMetadata>,
    derived: OnceLock<DownstreamDerivedRequestParts>,
}

impl DeferredDownstreamDerivedRequestParts {
    pub(super) fn new(
        uri: Uri,
        version: Version,
        headers: LazyHttpHeaders,
        connection_metadata: Option<DownstreamConnectionMetadata>,
    ) -> Self {
        Self {
            uri,
            version,
            headers,
            connection_metadata,
            derived: OnceLock::new(),
        }
    }

    fn parts(&self) -> &DownstreamDerivedRequestParts {
        self.derived.get_or_init(|| {
            DownstreamDerivedRequestParts::new(
                self.uri.clone(),
                self.version,
                self.headers.clone(),
                self.connection_metadata.clone(),
            )
        })
    }

    fn path(&self) -> &str {
        self.parts().path()
    }

    fn query(&self) -> &str {
        self.parts().query()
    }

    fn http_version(&self) -> &str {
        self.parts().http_version()
    }

    fn scheme(&self) -> &str {
        self.parts().scheme()
    }

    fn host(&self) -> &str {
        self.parts().host()
    }

    fn client_ip(&self) -> &str {
        self.parts().client_ip()
    }

    fn port(&self) -> u16 {
        self.parts().port()
    }
}

impl Clone for DeferredDownstreamDerivedRequestParts {
    fn clone(&self) -> Self {
        let derived = OnceLock::new();
        if let Some(parts) = self.derived.get() {
            let _ = derived.set(parts.clone());
        }
        Self {
            uri: self.uri.clone(),
            version: self.version,
            headers: self.headers.clone(),
            connection_metadata: self.connection_metadata.clone(),
            derived,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum RequestStringField {
    Static(String),
    Path(Arc<DeferredDownstreamDerivedRequestParts>),
    Query(Arc<DeferredDownstreamDerivedRequestParts>),
    HttpVersion(Arc<DeferredDownstreamDerivedRequestParts>),
    Scheme(Arc<DeferredDownstreamDerivedRequestParts>),
    Host(Arc<DeferredDownstreamDerivedRequestParts>),
    ClientIp(Arc<DeferredDownstreamDerivedRequestParts>),
}

impl RequestStringField {
    pub(crate) fn as_str(&self) -> &str {
        match self {
            Self::Static(value) => value.as_str(),
            Self::Path(parts) => parts.path(),
            Self::Query(parts) => parts.query(),
            Self::HttpVersion(parts) => parts.http_version(),
            Self::Scheme(parts) => parts.scheme(),
            Self::Host(parts) => parts.host(),
            Self::ClientIp(parts) => parts.client_ip(),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum RequestPortField {
    Static(u16),
    Derived(Arc<DeferredDownstreamDerivedRequestParts>),
}

impl RequestPortField {
    fn get(&self) -> u16 {
        match self {
            Self::Static(value) => *value,
            Self::Derived(parts) => parts.port(),
        }
    }
}

#[derive(Debug)]
pub struct HttpRequestContext {
    pub(crate) request_id: LazyRequestId,
    pub(crate) method: Method,
    pub(crate) path: RequestStringField,
    pub(crate) query: RequestStringField,
    pub(crate) http_version: RequestStringField,
    pub(crate) port: RequestPortField,
    pub(crate) scheme: RequestStringField,
    pub(crate) host: RequestStringField,
    pub(crate) client_ip: RequestStringField,
    pub(crate) body: Body,
    pub(crate) headers: LazyHttpHeaders,
}

#[derive(Clone, Debug)]
struct RawHttpHeader {
    name: Bytes,
    value: Bytes,
}

#[derive(Debug)]
struct LazyHttpHeadersInner {
    raw: Option<Arc<[RawHttpHeader]>>,
    parsed: OnceLock<HeaderMap>,
}

#[derive(Clone, Debug)]
pub struct LazyHttpHeaders {
    inner: Arc<LazyHttpHeadersInner>,
}

impl Default for LazyHttpHeaders {
    fn default() -> Self {
        HeaderMap::new().into()
    }
}

impl From<HeaderMap> for LazyHttpHeaders {
    fn from(headers: HeaderMap) -> Self {
        let parsed = OnceLock::new();
        let _ = parsed.set(headers);
        Self {
            inner: Arc::new(LazyHttpHeadersInner { raw: None, parsed }),
        }
    }
}

impl LazyHttpHeaders {
    pub(crate) fn from_raw_header_bytes(raw: Vec<(Bytes, Bytes)>) -> Self {
        let raw = raw
            .into_iter()
            .map(|(name, value)| RawHttpHeader { name, value })
            .collect::<Vec<_>>();
        Self {
            inner: Arc::new(LazyHttpHeadersInner {
                raw: Some(raw.into()),
                parsed: OnceLock::new(),
            }),
        }
    }

    pub(crate) fn headers(&self) -> &HeaderMap {
        self.inner.parsed.get_or_init(|| {
            let mut parsed = HeaderMap::new();
            if let Some(raw) = self.inner.raw.as_ref() {
                for header in raw.iter() {
                    let Ok(name) = HeaderName::from_bytes(&header.name) else {
                        continue;
                    };
                    let Ok(value) = HeaderValue::from_bytes(&header.value) else {
                        continue;
                    };
                    parsed.append(name, value);
                }
            }
            parsed
        })
    }

    pub(crate) fn get_str(&self, name: &str) -> Option<String> {
        if let Some(parsed) = self.inner.parsed.get() {
            return parsed
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
        }
        self.inner.raw.as_ref().and_then(|raw| {
            raw.iter().find_map(|header| {
                header
                    .name
                    .as_ref()
                    .eq_ignore_ascii_case(name.as_bytes())
                    .then(|| std::str::from_utf8(&header.value).ok().map(str::to_string))
                    .flatten()
            })
        })
    }

    pub(crate) fn contains_name(&self, name: &str) -> bool {
        if let Some(parsed) = self.inner.parsed.get() {
            return parsed.contains_key(name);
        }
        self.inner.raw.as_ref().is_some_and(|raw| {
            raw.iter()
                .any(|header| header.name.as_ref().eq_ignore_ascii_case(name.as_bytes()))
        })
    }

    pub(crate) fn header_contains_token(&self, name: &str, token: &str) -> bool {
        if let Some(parsed) = self.inner.parsed.get() {
            return parsed
                .get_all(name)
                .iter()
                .filter_map(|value| value.to_str().ok())
                .flat_map(|value| value.split(','))
                .map(str::trim)
                .any(|value| value.eq_ignore_ascii_case(token));
        }
        self.inner.raw.as_ref().is_some_and(|raw| {
            raw.iter()
                .filter(|header| header.name.as_ref().eq_ignore_ascii_case(name.as_bytes()))
                .filter_map(|header| std::str::from_utf8(&header.value).ok())
                .flat_map(|value| value.split(','))
                .map(str::trim)
                .any(|value| value.eq_ignore_ascii_case(token))
        })
    }

    pub(crate) fn content_length(&self) -> Option<u64> {
        self.get_str(CONTENT_LENGTH.as_str())
            .and_then(|value| value.parse::<u64>().ok())
    }

    pub(crate) fn for_each_header<F>(&self, mut f: F)
    where
        F: FnMut(&str, &[u8]),
    {
        if let Some(parsed) = self.inner.parsed.get() {
            for (name, value) in parsed {
                f(name.as_str(), value.as_bytes());
            }
            return;
        }
        if let Some(raw) = self.inner.raw.as_ref() {
            for header in raw.iter() {
                if let Ok(name) = std::str::from_utf8(&header.name) {
                    f(name, &header.value);
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct DownstreamConnectionMetadata {
    pub(crate) local_addr: SocketAddr,
    pub(crate) peer_addr: SocketAddr,
    pub(crate) secure: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum DownstreamHttpListenerGoal {
    #[default]
    None,
    #[cfg(feature = "tls")]
    Https,
}

impl DownstreamHttpListenerGoal {
    pub(crate) fn promotes_into_http(self) -> bool {
        !matches!(self, Self::None)
    }

    #[cfg(feature = "tls")]
    pub(crate) fn requires_tls(self) -> bool {
        matches!(self, Self::Https)
    }

    #[cfg(not(feature = "tls"))]
    pub(crate) fn requires_tls(self) -> bool {
        false
    }
}

#[derive(Clone, Debug)]
pub(crate) struct HttpRequestHead {
    pub(super) request_id: LazyRequestId,
    pub(super) method: Method,
    pub(super) path: RequestStringField,
    pub(super) query: RequestStringField,
    pub(super) http_version: RequestStringField,
    pub(super) port: RequestPortField,
    pub(super) scheme: RequestStringField,
    pub(super) host: RequestStringField,
    pub(super) client_ip: RequestStringField,
    pub(super) headers: LazyHttpHeaders,
}

impl HttpRequestHead {
    pub(crate) fn request_id(&self) -> &str {
        self.request_id.as_str()
    }

    pub(crate) fn method(&self) -> &Method {
        &self.method
    }

    pub(crate) fn path(&self) -> &str {
        self.path.as_str()
    }

    pub(crate) fn query(&self) -> &str {
        self.query.as_str()
    }

    pub(crate) fn http_version(&self) -> &str {
        self.http_version.as_str()
    }

    pub(crate) fn port(&self) -> u16 {
        self.port.get()
    }

    pub(crate) fn scheme(&self) -> &str {
        self.scheme.as_str()
    }

    pub(crate) fn host(&self) -> &str {
        self.host.as_str()
    }

    pub(crate) fn client_ip(&self) -> &str {
        self.client_ip.as_str()
    }

    pub(crate) fn headers(&self) -> &HeaderMap {
        self.headers.headers()
    }

    pub(crate) fn lazy_headers(&self) -> &LazyHttpHeaders {
        &self.headers
    }

    pub(super) fn scheme_field(&self) -> &RequestStringField {
        &self.scheme
    }

    pub(super) fn host_field(&self) -> &RequestStringField {
        &self.host
    }

    pub(super) fn http_version_field(&self) -> &RequestStringField {
        &self.http_version
    }
}

pub(crate) fn http_version_label(version: Version) -> &'static str {
    if http2::supports_response_version(version) {
        http2::response_version_label()
    } else {
        match version {
            Version::HTTP_09 => "0.9",
            Version::HTTP_10 => "1.0",
            Version::HTTP_11 => "1.1",
            Version::HTTP_3 => "3",
            _ => "1.1",
        }
    }
}

pub(crate) fn build_downstream_http_request_context(
    request_id: LazyRequestId,
    parts: axum::http::request::Parts,
    body: Body,
    connection_metadata: Option<&DownstreamConnectionMetadata>,
) -> HttpRequestContext {
    build_downstream_http_request_context_from_components(
        request_id,
        parts.method,
        parts.uri,
        parts.version,
        body,
        parts.headers.into(),
        connection_metadata,
    )
}

pub(crate) fn build_downstream_http_request_context_from_components(
    request_id: LazyRequestId,
    method: Method,
    uri: axum::http::Uri,
    version: Version,
    body: Body,
    headers: LazyHttpHeaders,
    connection_metadata: Option<&DownstreamConnectionMetadata>,
) -> HttpRequestContext {
    let derived = Arc::new(DeferredDownstreamDerivedRequestParts::new(
        uri.clone(),
        version,
        headers.clone(),
        connection_metadata.cloned(),
    ));
    HttpRequestContext {
        request_id,
        method,
        path: RequestStringField::Path(derived.clone()),
        query: RequestStringField::Query(derived.clone()),
        http_version: RequestStringField::HttpVersion(derived.clone()),
        port: RequestPortField::Derived(derived.clone()),
        scheme: RequestStringField::Scheme(derived.clone()),
        host: RequestStringField::Host(derived.clone()),
        client_ip: RequestStringField::ClientIp(derived),
        body,
        headers,
    }
}

fn resolve_downstream_request_scheme(
    uri: &axum::http::Uri,
    headers: &LazyHttpHeaders,
    connection_metadata: Option<&DownstreamConnectionMetadata>,
) -> String {
    if let Some(scheme) = uri.scheme_str() {
        return scheme.to_string();
    }
    if let Some(forwarded) = headers
        .get_str("x-forwarded-proto")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        return forwarded;
    }
    if let Some(connection_metadata) = connection_metadata
        && connection_metadata.secure
    {
        return "https".to_string();
    }
    "http".to_string()
}

fn resolve_downstream_request_port(
    uri: &axum::http::Uri,
    headers: &LazyHttpHeaders,
    scheme: &str,
    connection_metadata: Option<&DownstreamConnectionMetadata>,
) -> u16 {
    if let Some(port) = uri.port_u16() {
        return port;
    }
    if let Some(host_header) = headers
        .get_str(HOST.as_str())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        && let Ok(authority) = host_header.parse::<axum::http::uri::Authority>()
        && let Some(port) = authority.port_u16()
    {
        return port;
    }
    if let Some(connection_metadata) = connection_metadata {
        return connection_metadata.local_addr.port();
    }
    if scheme.eq_ignore_ascii_case("https") {
        443
    } else {
        80
    }
}

fn resolve_downstream_request_host(uri: &axum::http::Uri, headers: &LazyHttpHeaders) -> String {
    if let Some(host) = headers
        .get_str(HOST.as_str())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        return host;
    }
    uri.authority()
        .map(|authority| authority.as_str().to_string())
        .unwrap_or_default()
}

fn resolve_downstream_request_client_ip(
    headers: &LazyHttpHeaders,
    connection_metadata: Option<&DownstreamConnectionMetadata>,
) -> String {
    if let Some(value) = headers.get_str("x-forwarded-for") {
        let first = value
            .split(',')
            .map(str::trim)
            .find(|candidate| !candidate.is_empty())
            .unwrap_or_default();
        if !first.is_empty() {
            return first.to_string();
        }
    }
    headers
        .get_str("x-real-ip")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| connection_metadata.map(|metadata| metadata.peer_addr.ip().to_string()))
        .unwrap_or_default()
}
