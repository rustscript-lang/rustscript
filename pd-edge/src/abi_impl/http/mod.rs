#[cfg(feature = "http")]
pub(crate) mod exchange;
pub(crate) mod fast_path;
mod helpers;
pub(crate) mod outbound_http1;
#[cfg(feature = "http")]
mod request;
#[cfg(feature = "http")]
pub(crate) mod response;
pub(crate) mod state;
pub(crate) mod version;

pub use state::{HttpRequestContext, LazyHttpHeaders, ProxyVmContext, SharedProxyVmContext};
#[cfg(feature = "webrtc")]
pub(crate) use state::{
    allocate_webrtc_connection_handle, default_upstream_webrtc_connection_handle,
    webrtc_connection_exists,
};
