use std::{fmt, time::Duration};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{
    Method, Request, StatusCode, Uri,
    body::Incoming,
    header::{ACCEPT, CONTENT_TYPE},
};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::TokioExecutor,
};
use serde::{Serialize, de::DeserializeOwned};

type ControlPlaneHyperClient = Client<HttpsConnector<HttpConnector>, Full<Bytes>>;

#[derive(Clone)]
pub struct ControlPlaneHttpClient {
    inner: ControlPlaneHyperClient,
}

impl ControlPlaneHttpClient {
    pub fn new(max_idle_per_host: usize) -> Self {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let connector = HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_http1()
            .enable_http2()
            .build();
        let inner = Client::builder(TokioExecutor::new())
            .pool_max_idle_per_host(max_idle_per_host.max(1))
            .build(connector);
        Self { inner }
    }

    pub async fn post_json<T: Serialize>(
        &self,
        url: &str,
        timeout: Duration,
        value: &T,
    ) -> Result<ControlPlaneHttpResponse, ControlPlaneHttpClientError> {
        let uri = url
            .parse::<Uri>()
            .map_err(ControlPlaneHttpClientError::InvalidUri)?;
        let body = serde_json::to_vec(value).map_err(ControlPlaneHttpClientError::Serialize)?;
        let request = Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json")
            .body(Full::new(Bytes::from(body)))
            .map_err(ControlPlaneHttpClientError::BuildRequest)?;
        let response = tokio::time::timeout(timeout, self.inner.request(request))
            .await
            .map_err(|_| ControlPlaneHttpClientError::Timeout)?
            .map_err(ControlPlaneHttpClientError::Transport)?;
        ControlPlaneHttpResponse::from_hyper(response, timeout).await
    }
}

pub struct ControlPlaneHttpResponse {
    status: StatusCode,
    body: Bytes,
}

impl ControlPlaneHttpResponse {
    async fn from_hyper(
        response: hyper::Response<Incoming>,
        timeout: Duration,
    ) -> Result<Self, ControlPlaneHttpClientError> {
        let status = response.status();
        let body = tokio::time::timeout(timeout, async move {
            response
                .into_body()
                .collect()
                .await
                .map(|collected| collected.to_bytes())
        })
        .await
        .map_err(|_| ControlPlaneHttpClientError::Timeout)?
        .map_err(ControlPlaneHttpClientError::Collect)?;
        Ok(Self { status, body })
    }

    pub fn status(&self) -> StatusCode {
        self.status
    }

    pub fn json<T: DeserializeOwned>(&self) -> Result<T, ControlPlaneHttpClientError> {
        serde_json::from_slice(&self.body).map_err(ControlPlaneHttpClientError::Deserialize)
    }
}

#[derive(Debug)]
pub enum ControlPlaneHttpClientError {
    InvalidUri(hyper::http::uri::InvalidUri),
    Serialize(serde_json::Error),
    BuildRequest(hyper::http::Error),
    Transport(hyper_util::client::legacy::Error),
    Collect(hyper::Error),
    Deserialize(serde_json::Error),
    Timeout,
}

impl fmt::Display for ControlPlaneHttpClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUri(err) => write!(f, "invalid uri: {err}"),
            Self::Serialize(err) => write!(f, "serialize error: {err}"),
            Self::BuildRequest(err) => write!(f, "request build error: {err}"),
            Self::Transport(err) => write!(f, "transport error: {err}"),
            Self::Collect(err) => write!(f, "response read error: {err}"),
            Self::Deserialize(err) => write!(f, "response decode error: {err}"),
            Self::Timeout => write!(f, "request timed out"),
        }
    }
}

impl std::error::Error for ControlPlaneHttpClientError {}
