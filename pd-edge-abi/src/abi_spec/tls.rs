#[pd_host_function(name = "tls::session::from_socket")]
fn tls_session_from_socket(stream: i64) -> i64 {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "tls::session::is_present")]
fn tls_session_is_present(session: i64) -> bool {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "tls::session::handshake")]
fn tls_session_handshake(session: i64) -> bool {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "tls::session::set_alpn")]
fn tls_session_set_alpn(session: i64, protocols: &str) {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "tls::session::set_verify")]
fn tls_session_set_verify(session: i64, verify: bool) {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "tls::session::set_verify_hostname")]
fn tls_session_set_verify_hostname(session: i64, verify: bool) {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "tls::session::set_trusted_certificate")]
fn tls_session_set_trusted_certificate(session: i64, certificate_pem: &str) {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "tls::session::set_certificate")]
fn tls_session_set_certificate(session: i64, certificate_pem: &str) {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "tls::session::set_private_key")]
fn tls_session_set_private_key(session: i64, private_key_pem: &str) {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "tls::session::set_sni")]
fn tls_session_set_sni(session: i64, enabled: bool) {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "tls::session::set_min_version")]
fn tls_session_set_min_version(session: i64, version: &str) {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "tls::session::set_max_version")]
fn tls_session_set_max_version(session: i64, version: &str) {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "tls::session::get_peer_name")]
fn tls_session_get_peer_name(session: i64) -> String {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "tls::session::get_server_name")]
fn tls_session_get_server_name(session: i64) -> String {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "tls::session::get_alpn")]
fn tls_session_get_alpn(session: i64) -> String {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "tls::session::get_phase")]
fn tls_session_get_phase(session: i64) -> String {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "tls::session::get_peer_certificate")]
fn tls_session_get_peer_certificate(session: i64) -> String {
    unreachable!("abi declaration only")
}

#[pd_host_function(name = "tls::session::is_session_reused")]
fn tls_session_is_session_reused(session: i64) -> bool {
    unreachable!("abi declaration only")
}
