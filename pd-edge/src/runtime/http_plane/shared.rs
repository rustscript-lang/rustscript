use std::time::Instant;

use axum::{body::Body, extract::Request, middleware::Next, response::Response};
use tracing::info;

use crate::logging::{category_access, method_label, status_label};

pub(super) async fn access_log_middleware(request: Request, next: Next) -> Response<Body> {
    let method = request.method().clone();
    let uri = request.uri().clone();
    let started = Instant::now();
    let response = next.run(request).await;
    let elapsed_ms = started.elapsed().as_millis();
    let status = response.status();

    info!(
        "{} {} {} {} {}ms",
        category_access(),
        method_label(method.as_str()),
        status_label(status.as_u16()),
        uri,
        elapsed_ms
    );

    response
}
