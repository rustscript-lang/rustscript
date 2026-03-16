mod admin_api;
mod proxy_path;
mod shared;

pub use admin_api::build_admin_app;
#[cfg(feature = "http3")]
pub use proxy_path::serve_http3_proxy;
pub(crate) use proxy_path::{
    auto_promote_downstream_listener_goal_into_http_request,
    maybe_auto_promote_downstream_listener_goal_into_http_request,
    promote_transport_context_into_http_request, scoped_http_host_call_can_run_synchronously,
};
pub use proxy_path::{build_http_proxy_app, serve_http_proxy, serve_https_proxy};
