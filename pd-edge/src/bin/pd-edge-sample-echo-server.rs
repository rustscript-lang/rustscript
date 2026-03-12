use std::{env, net::SocketAddr};

use edge::{
    init_logging,
    sample_echo::{SampleEchoServerConfig, spawn_sample_echo_server},
};
use tracing::{info, warn};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = match parse_cli_args() {
        Ok(CliAction::Run(cli)) => cli,
        Ok(CliAction::Help) => {
            print_cli_help();
            return Ok(());
        }
        Ok(CliAction::Version) => {
            println!("{}", binary_version_text());
            return Ok(());
        }
        Err(err) => {
            eprintln!("error: {err}\n");
            print_cli_help();
            return Err(err.into());
        }
    };

    init_logging()?;
    info!("{}", binary_version_text());

    let server = spawn_sample_echo_server((*cli).into_config()).await?;
    info!("tcp echo listening on {}", server.addresses.tcp);
    info!("udp echo listening on {}", server.addresses.udp);

    if let Some(addr) = server.addresses.tls {
        info!("tls echo listening on {}", addr);
    } else {
        warn!("tls echo disabled in this build; enable the `tls` feature");
    }

    if let Some(addr) = server.addresses.http {
        #[cfg(feature = "http2")]
        info!(
            "http echo listening on http://{} (HTTP/1.1 + h2c prior knowledge)",
            addr
        );
        #[cfg(not(feature = "http2"))]
        info!("http echo listening on http://{}", addr);
    } else {
        warn!("http echo disabled in this build; enable the `http` feature");
    }

    if let Some(addr) = server.addresses.https {
        #[cfg(feature = "http2")]
        info!(
            "https echo listening on https://{} (ALPN: h2, http/1.1)",
            addr
        );
        #[cfg(not(feature = "http2"))]
        info!("https echo listening on https://{}", addr);
    } else {
        warn!("https echo disabled in this build; enable the `tls` feature");
    }

    if let Some(addr) = server.addresses.websocket {
        info!("websocket echo listening on ws://{}", addr);
    } else {
        warn!("websocket echo disabled in this build; enable the `websocket` feature");
    }

    if let Some(addr) = server.addresses.websocket_tls {
        info!("secure websocket echo listening on wss://{}", addr);
    } else {
        warn!(
            "secure websocket echo disabled in this build; enable the `websocket` and `tls` features"
        );
    }

    if let Some(addr) = server.addresses.webrtc {
        info!("webrtc signaling echo listening on http://{}/offer", addr);
    } else {
        warn!("webrtc echo disabled in this build; enable the `webrtc` feature");
    }
    info!(
        "forward proxy listening on {} (CONNECT tunnel)",
        server.addresses.forward_proxy
    );

    info!("sample echo server is running; press Ctrl+C to stop");
    tokio::signal::ctrl_c().await?;
    info!("shutting down sample echo server");
    drop(server);
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CliArgs {
    tcp_addr: SocketAddr,
    udp_addr: SocketAddr,
    tls_addr: SocketAddr,
    http_addr: SocketAddr,
    https_addr: SocketAddr,
    websocket_addr: SocketAddr,
    websocket_tls_addr: SocketAddr,
    webrtc_addr: SocketAddr,
    forward_proxy_addr: SocketAddr,
}

impl Default for CliArgs {
    fn default() -> Self {
        let config = SampleEchoServerConfig::default();
        Self {
            tcp_addr: config.tcp_addr,
            udp_addr: config.udp_addr,
            tls_addr: config.tls_addr,
            http_addr: config.http_addr,
            https_addr: config.https_addr,
            websocket_addr: config.websocket_addr,
            websocket_tls_addr: config.websocket_tls_addr,
            webrtc_addr: config.webrtc_addr,
            forward_proxy_addr: config.forward_proxy_addr,
        }
    }
}

impl CliArgs {
    fn into_config(self) -> SampleEchoServerConfig {
        SampleEchoServerConfig {
            tcp_addr: self.tcp_addr,
            udp_addr: self.udp_addr,
            tls_addr: self.tls_addr,
            http_addr: self.http_addr,
            https_addr: self.https_addr,
            websocket_addr: self.websocket_addr,
            websocket_tls_addr: self.websocket_tls_addr,
            webrtc_addr: self.webrtc_addr,
            forward_proxy_addr: self.forward_proxy_addr,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum CliAction {
    Run(Box<CliArgs>),
    Help,
    Version,
}

fn parse_cli_args() -> Result<CliAction, String> {
    parse_cli_args_from(env::args().skip(1))
}

fn parse_cli_args_from<I>(args: I) -> Result<CliAction, String>
where
    I: IntoIterator<Item = String>,
{
    let mut args = args.into_iter().peekable();
    let mut cli = CliArgs::default();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(CliAction::Help),
            "-V" | "--version" => return Ok(CliAction::Version),
            "--tcp-addr" => {
                cli.tcp_addr =
                    parse_socket_addr("--tcp-addr", &next_arg_value("--tcp-addr", &mut args)?)?;
            }
            "--udp-addr" => {
                cli.udp_addr =
                    parse_socket_addr("--udp-addr", &next_arg_value("--udp-addr", &mut args)?)?;
            }
            "--tls-addr" => {
                cli.tls_addr =
                    parse_socket_addr("--tls-addr", &next_arg_value("--tls-addr", &mut args)?)?;
            }
            "--http-addr" => {
                cli.http_addr =
                    parse_socket_addr("--http-addr", &next_arg_value("--http-addr", &mut args)?)?;
            }
            "--https-addr" => {
                cli.https_addr =
                    parse_socket_addr("--https-addr", &next_arg_value("--https-addr", &mut args)?)?;
            }
            "--websocket-addr" | "--ws-addr" => {
                let flag = if arg == "--websocket-addr" {
                    "--websocket-addr"
                } else {
                    "--ws-addr"
                };
                cli.websocket_addr = parse_socket_addr(flag, &next_arg_value(flag, &mut args)?)?;
            }
            "--websocket-tls-addr" | "--wss-addr" => {
                let flag = if arg == "--websocket-tls-addr" {
                    "--websocket-tls-addr"
                } else {
                    "--wss-addr"
                };
                cli.websocket_tls_addr =
                    parse_socket_addr(flag, &next_arg_value(flag, &mut args)?)?;
            }
            "--webrtc-addr" => {
                cli.webrtc_addr = parse_socket_addr(
                    "--webrtc-addr",
                    &next_arg_value("--webrtc-addr", &mut args)?,
                )?;
            }
            "--forward-proxy-addr" => {
                cli.forward_proxy_addr = parse_socket_addr(
                    "--forward-proxy-addr",
                    &next_arg_value("--forward-proxy-addr", &mut args)?,
                )?;
            }
            _ => return Err(format!("unknown argument: {arg}")),
        }
    }

    Ok(CliAction::Run(Box::new(cli)))
}

fn parse_socket_addr(flag: &str, value: &str) -> Result<SocketAddr, String> {
    value
        .parse::<SocketAddr>()
        .map_err(|_| format!("invalid {flag}: {value}"))
}

fn next_arg_value(
    flag: &str,
    args: &mut std::iter::Peekable<impl Iterator<Item = String>>,
) -> Result<String, String> {
    let value = args
        .next()
        .ok_or_else(|| format!("missing value for {flag}"))?;
    if value.trim().is_empty() {
        return Err(format!("value for {flag} cannot be empty"));
    }
    Ok(value)
}

fn print_cli_help() {
    eprintln!(concat!(
        "Usage: pd-edge-sample-echo-server [options]\n\n",
        "Options:\n",
        "  --tcp-addr <ADDR>                        TCP echo listen address (default: 127.0.0.1:7001)\n",
        "  --udp-addr <ADDR>                        UDP echo listen address (default: 127.0.0.1:7002)\n",
        "  --tls-addr <ADDR>                        TLS echo listen address (default: 127.0.0.1:7003)\n",
        "  --http-addr <ADDR>                       HTTP echo listen address (default: 127.0.0.1:7004)\n",
        "  --https-addr <ADDR>                      HTTPS echo listen address (default: 127.0.0.1:7005)\n",
        "  --websocket-addr, --ws-addr <ADDR>       WebSocket echo listen address (default: 127.0.0.1:7006)\n",
        "  --websocket-tls-addr, --wss-addr <ADDR>  Secure WebSocket echo listen address (default: 127.0.0.1:7007)\n",
        "  --webrtc-addr <ADDR>                     WebRTC signaling listen address (default: 127.0.0.1:7008)\n",
        "  --forward-proxy-addr <ADDR>              CONNECT forward proxy listen address (default: 127.0.0.1:7009)\n",
        "  -V, --version                            Show version with git metadata\n",
        "  -h, --help                               Show this help\n\n",
        "Notes:\n",
        "  The TLS, HTTPS, and WSS listeners use a generated self-signed certificate.\n",
        "  With feature `http2`, the HTTP listener also accepts cleartext h2c prior knowledge.\n",
        "  With feature `http2`, the HTTPS listener negotiates h2 or HTTP/1.1 via ALPN.\n",
        "  Without feature `http2`, the HTTP and HTTPS listeners serve HTTP/1.1 only.\n",
        "  The forward proxy listener accepts CONNECT and then tunnels raw TCP bytes.\n",
        "  The WebRTC listener serves signaling over HTTP POST /offer and echoes data-channel messages.\n",
        "  Feature-gated listeners are only enabled when the corresponding crate feature is compiled in.\n",
    ));
}

fn binary_version_text() -> String {
    let binary = env!("CARGO_BIN_NAME");
    let git_tag = option_env!("PD_BUILD_GIT_TAG").unwrap_or("untagged");
    let git_commit = option_env!("PD_BUILD_GIT_COMMIT").unwrap_or("unknown");
    let git_dirty = option_env!("PD_BUILD_GIT_DIRTY").unwrap_or("false");
    let dirty = matches!(git_dirty, "true" | "1" | "yes" | "dirty");

    if dirty {
        format!("{binary} {git_tag} (dirty commit: {git_commit})")
    } else {
        format!("{binary} {git_tag}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cli_args_from_handles_help_and_version() {
        assert_eq!(
            parse_cli_args_from(["--help".to_string()]).expect("help should parse"),
            CliAction::Help
        );
        assert_eq!(
            parse_cli_args_from(["-V".to_string()]).expect("version should parse"),
            CliAction::Version
        );
    }

    #[test]
    fn parse_cli_args_from_parses_custom_addresses() {
        let action = parse_cli_args_from([
            "--tcp-addr".to_string(),
            "127.0.0.1:9101".to_string(),
            "--udp-addr".to_string(),
            "127.0.0.1:9102".to_string(),
            "--tls-addr".to_string(),
            "127.0.0.1:9103".to_string(),
            "--http-addr".to_string(),
            "127.0.0.1:9104".to_string(),
            "--https-addr".to_string(),
            "127.0.0.1:9105".to_string(),
            "--ws-addr".to_string(),
            "127.0.0.1:9106".to_string(),
            "--wss-addr".to_string(),
            "127.0.0.1:9107".to_string(),
            "--webrtc-addr".to_string(),
            "127.0.0.1:9108".to_string(),
            "--forward-proxy-addr".to_string(),
            "127.0.0.1:9109".to_string(),
        ])
        .expect("cli should parse");

        let CliAction::Run(cli) = action else {
            panic!("expected run action");
        };
        assert_eq!(
            cli.tcp_addr,
            "127.0.0.1:9101".parse::<SocketAddr>().expect("valid addr")
        );
        assert_eq!(
            cli.udp_addr,
            "127.0.0.1:9102".parse::<SocketAddr>().expect("valid addr")
        );
        assert_eq!(
            cli.tls_addr,
            "127.0.0.1:9103".parse::<SocketAddr>().expect("valid addr")
        );
        assert_eq!(
            cli.http_addr,
            "127.0.0.1:9104".parse::<SocketAddr>().expect("valid addr")
        );
        assert_eq!(
            cli.https_addr,
            "127.0.0.1:9105".parse::<SocketAddr>().expect("valid addr")
        );
        assert_eq!(
            cli.websocket_addr,
            "127.0.0.1:9106".parse::<SocketAddr>().expect("valid addr")
        );
        assert_eq!(
            cli.websocket_tls_addr,
            "127.0.0.1:9107".parse::<SocketAddr>().expect("valid addr")
        );
        assert_eq!(
            cli.webrtc_addr,
            "127.0.0.1:9108".parse::<SocketAddr>().expect("valid addr")
        );
        assert_eq!(
            cli.forward_proxy_addr,
            "127.0.0.1:9109".parse::<SocketAddr>().expect("valid addr")
        );
    }

    #[test]
    fn parse_cli_args_from_rejects_unknown_argument() {
        let err = parse_cli_args_from(["--nope".to_string()]).expect_err("unknown arg should fail");
        assert!(err.contains("unknown argument"));
    }

    #[test]
    fn parse_cli_args_from_rejects_invalid_address() {
        let err = parse_cli_args_from(["--tcp-addr".to_string(), "not-an-addr".to_string()])
            .expect_err("invalid addr should fail");
        assert!(err.contains("invalid --tcp-addr"));
    }
}
