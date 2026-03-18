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
