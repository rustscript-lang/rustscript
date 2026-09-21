use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Instant;

use hyper_util::client::legacy::connect::dns::Name;
use tower_service::Service;

use super::config::HttpConfig;
use crate::vm::{VmError, VmResult};

/// URL scheme family admitted by a protocol adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SchemeFamily {
    Http,
}

impl SchemeFamily {
    fn accepts(self, scheme: &str) -> bool {
        match self {
            Self::Http => matches!(scheme, "http" | "https"),
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct ResolvedTarget {
    pub(super) host: String,
    pub(super) address: SocketAddr,
}

/// Shared admission state for every connection-oriented HTTP adapter.
#[derive(Clone, Debug)]
pub(super) struct ConnectionAdmission {
    max_in_flight: Arc<AtomicUsize>,
    in_flight: Arc<AtomicUsize>,
    open: Arc<AtomicBool>,
}

impl ConnectionAdmission {
    pub(super) fn new(max_in_flight: usize) -> Self {
        Self {
            max_in_flight: Arc::new(AtomicUsize::new(max_in_flight)),
            in_flight: Arc::new(AtomicUsize::new(0)),
            open: Arc::new(AtomicBool::new(true)),
        }
    }

    pub(super) fn set_max_in_flight(&self, max_in_flight: usize) {
        self.max_in_flight.store(max_in_flight, Ordering::Release);
    }

    pub(super) fn max_in_flight(&self) -> usize {
        self.max_in_flight.load(Ordering::Acquire)
    }

    pub(super) fn close(&self) {
        self.open.store(false, Ordering::Release);
    }

    pub(super) fn acquire(&self) -> VmResult<ConnectionPermit> {
        if !self.open.load(Ordering::Acquire) {
            return Err(VmError::HostError(
                "HTTP worker resource admission is closed".to_string(),
            ));
        }
        let mut active = self.in_flight.load(Ordering::Acquire);
        loop {
            let max_in_flight = self.max_in_flight.load(Ordering::Acquire);
            if active >= max_in_flight {
                return Err(VmError::HostError(format!(
                    "HTTP in-flight request limit of {max_in_flight} was reached"
                )));
            }
            match self.in_flight.compare_exchange_weak(
                active,
                active + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    let permit = ConnectionPermit {
                        in_flight: Arc::clone(&self.in_flight),
                    };
                    if self.open.load(Ordering::Acquire) {
                        return Ok(permit);
                    }
                    drop(permit);
                    return Err(VmError::HostError(
                        "HTTP worker resource admission is closed".to_string(),
                    ));
                }
                Err(observed) => active = observed,
            }
        }
    }
}

/// Releases one shared connection slot when its embedding-owned future retires.
pub(super) struct ConnectionPermit {
    in_flight: Arc<AtomicUsize>,
}

impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        self.in_flight.fetch_sub(1, Ordering::AcqRel);
    }
}

/// DNS resolver used by Hyper's connector.
///
/// The request path validates scheme, host, port, and every current DNS answer
/// before dispatch. Hyper invokes this resolver whenever its pool needs a new
/// connection, so the address used by the actual socket is checked again and
/// cannot bypass the private-address policy through DNS rebinding.
#[derive(Clone, Debug)]
pub(super) struct PolicyResolver {
    allow_private_ips: bool,
}

impl PolicyResolver {
    pub(super) fn new(config: &HttpConfig) -> Self {
        Self {
            allow_private_ips: config.allow_private_ips,
        }
    }
}

impl Service<Name> for PolicyResolver {
    type Response = std::vec::IntoIter<SocketAddr>;
    type Error = std::io::Error;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, name: Name) -> Self::Future {
        let host = name.as_str().to_string();
        let allow_private_ips = self.allow_private_ips;
        Box::pin(async move {
            let addresses = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .collect::<Vec<_>>();
            let config = HttpConfig {
                allow_private_ips,
                ..HttpConfig::default()
            };
            validate_resolved_addresses(&config, &addresses).map_err(|error| {
                std::io::Error::new(std::io::ErrorKind::PermissionDenied, error.to_string())
            })?;
            Ok(addresses.into_iter())
        })
    }
}

pub(super) fn validate_url_policy(
    config: &HttpConfig,
    family: SchemeFamily,
    url: &url::Url,
) -> VmResult<(String, u16)> {
    validate_url_structure(url)?;
    let scheme = url.scheme().to_ascii_lowercase();
    if !family.accepts(&scheme)
        || !config
            .allowed_schemes
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(&scheme))
    {
        return Err(VmError::HostError(format!(
            "HTTP URL scheme '{scheme}' is not allowed",
        )));
    }
    let host = url
        .host_str()
        .expect("structurally validated HTTP URL must have a host");
    if !config
        .allowed_hosts
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(host))
    {
        return Err(VmError::HostError(
            "HTTP target host is not allowed".to_string(),
        ));
    }
    let port = url
        .port_or_known_default()
        .ok_or_else(|| VmError::HostError("HTTP URL has no known port".to_string()))?;
    if !config.allowed_ports.contains(&port) {
        return Err(VmError::HostError(format!(
            "HTTP target port {port} is not allowed",
        )));
    }
    Ok((host.to_string(), port))
}

fn validate_url_structure(url: &url::Url) -> VmResult<()> {
    if !url.username().is_empty() || url.password().is_some() {
        return Err(VmError::HostError(
            "HTTP URL userinfo is not allowed".to_string(),
        ));
    }
    url.host_str()
        .ok_or_else(|| VmError::HostError("HTTP URL has no host".to_string()))?;
    Ok(())
}

pub(super) async fn resolve_url(
    config: &HttpConfig,
    family: SchemeFamily,
    url: &url::Url,
) -> VmResult<ResolvedTarget> {
    let (host, port) = validate_url_policy(config, family, url)?;
    let addresses = if let Ok(host_ip) = host.parse::<IpAddr>() {
        vec![SocketAddr::new(host_ip, port)]
    } else {
        tokio::net::lookup_host((host.as_str(), port))
            .await
            .map_err(|error| VmError::HostError(format!("HTTP host resolution failed: {error}")))?
            .collect::<Vec<_>>()
    };
    validate_resolved_addresses(config, &addresses)?;
    let address = addresses
        .first()
        .copied()
        .ok_or_else(|| VmError::HostError("HTTP target resolves to a restricted IP".to_string()))?;
    Ok(ResolvedTarget { host, address })
}

pub(super) fn validate_resolved_addresses(
    config: &HttpConfig,
    addresses: &[SocketAddr],
) -> VmResult<()> {
    if addresses.is_empty()
        || (!config.allow_private_ips
            && addresses
                .iter()
                .any(|address| is_restricted_ip(address.ip())))
    {
        return Err(VmError::HostError(
            "HTTP target resolves to a restricted IP".to_string(),
        ));
    }
    Ok(())
}

pub(super) fn is_restricted_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let octets = ip.octets();
            matches!(octets[0], 0 | 10 | 127)
                || (octets[0] == 100 && (64..=127).contains(&octets[1]))
                || (octets[0] == 169 && octets[1] == 254)
                || (octets[0] == 172 && (16..=31).contains(&octets[1]))
                || (octets[0] == 192
                    && matches!(
                        (octets[1], octets[2]),
                        (0, 0) | (0, 2) | (31, 196) | (52, 193) | (88, 99) | (168, _) | (175, 48)
                    ))
                || (octets[0] == 198
                    && ((18..=19).contains(&octets[1]) || (octets[1] == 51 && octets[2] == 100)))
                || (octets[0] == 203 && octets[1] == 0 && octets[2] == 113)
                || octets[0] >= 224
        }
        IpAddr::V6(ip) => {
            if let Some(mapped) = ip.to_ipv4_mapped() {
                return is_restricted_ip(IpAddr::V4(mapped));
            }
            let segments = ip.segments();
            let outside_global_unicast = segments[0] & 0xe000 != 0x2000;
            let protocol_assignments = segments[0] == 0x2001 && segments[1] <= 0x01ff;
            let documentation = (segments[0] == 0x2001 && segments[1] == 0x0db8)
                || (segments[0] == 0x3fff && segments[1] & 0xf000 == 0);
            let six_to_four = segments[0] == 0x2002;
            let direct_delegation_as112 =
                segments[0] == 0x2620 && segments[1] == 0x004f && segments[2] == 0x8000;
            outside_global_unicast
                || protocol_assignments
                || documentation
                || six_to_four
                || direct_delegation_as112
        }
    }
}

pub(super) fn phase_deadline(absolute: Instant, phase_limit: std::time::Duration) -> Instant {
    phase_deadline_at(Instant::now(), absolute, phase_limit)
}

fn phase_deadline_at(now: Instant, absolute: Instant, phase_limit: std::time::Duration) -> Instant {
    absolute.min(now.checked_add(phase_limit).unwrap_or(absolute))
}

pub(super) async fn with_deadline<T>(
    deadline: Instant,
    future: impl std::future::Future<Output = VmResult<T>>,
) -> VmResult<T> {
    tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), future)
        .await
        .map_err(|_| VmError::HostError("HTTP request deadline exceeded".to_string()))?
}

pub(super) fn request_deadline(timeout: std::time::Duration) -> VmResult<Instant> {
    Instant::now().checked_add(timeout).ok_or_else(|| {
        VmError::HostError("HTTP request_timeout cannot form a deadline".to_string())
    })
}

#[cfg(test)]
pub(super) fn validate_url(
    config: &HttpConfig,
    family: SchemeFamily,
    url: &url::Url,
) -> VmResult<Option<SocketAddr>> {
    let (host, port) = validate_url_policy(config, family, url)?;
    if config.allow_private_ips {
        return Ok(None);
    }
    if let Ok(host_ip) = host.parse::<IpAddr>() {
        validate_resolved_addresses(config, &[SocketAddr::new(host_ip, port)])?;
        return Ok(None);
    }
    use std::net::ToSocketAddrs;
    let addresses = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|error| VmError::HostError(format!("HTTP host resolution failed: {error}")))?
        .collect::<Vec<_>>();
    validate_resolved_addresses(config, &addresses)?;
    Ok(addresses.first().copied())
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::phase_deadline_at;

    #[test]
    fn opening_phase_deadline_cannot_reset_absolute_budget_per_hop() {
        let start = Instant::now();
        let absolute = start + Duration::from_secs(10);
        assert_eq!(
            phase_deadline_at(
                start + Duration::from_secs(1),
                absolute,
                Duration::from_secs(10)
            ),
            absolute
        );
        assert_eq!(
            phase_deadline_at(
                start + Duration::from_secs(8),
                absolute,
                Duration::from_secs(10)
            ),
            absolute
        );
        assert_eq!(
            phase_deadline_at(
                start + Duration::from_secs(11),
                absolute,
                Duration::from_secs(10)
            ),
            absolute
        );
    }
}
