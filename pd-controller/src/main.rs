use std::{env, net::SocketAddr};

use pd_controller::{ControllerConfig, ControllerState, build_controller_app};
use tracing::info;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if wants_version_flag() {
        println!("{}", binary_version_text());
        return Ok(());
    }

    init_logging();
    info!("{}", binary_version_text());

    let addr = parse_addr("CONTROLLER_ADDR", "0.0.0.0:9100")?;
    let config = ControllerConfig {
        default_poll_interval_ms: parse_u64("CONTROLLER_DEFAULT_POLL_MS", 1_000)?,
        max_result_history: parse_usize("CONTROLLER_MAX_RESULT_HISTORY", 200)?,
        state_path: parse_state_path("CONTROLLER_STATE_PATH", ".pd-controller/state.json"),
    };

    let state = ControllerState::new(config);
    let app = build_controller_app(state);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!("controller listening on http://{}", listener.local_addr()?);
    axum::serve(listener, app).await?;
    Ok(())
}

fn init_logging() {
    let env_filter =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    tracing_subscriber::fmt().with_env_filter(env_filter).init();
}

fn parse_addr(key: &str, default: &str) -> Result<SocketAddr, Box<dyn std::error::Error>> {
    let value = env::var(key).unwrap_or_else(|_| default.to_string());
    Ok(value.parse()?)
}

fn parse_u64(key: &str, default: u64) -> Result<u64, Box<dyn std::error::Error>> {
    match env::var(key) {
        Ok(value) => Ok(value.parse()?),
        Err(_) => Ok(default),
    }
}

fn parse_usize(key: &str, default: usize) -> Result<usize, Box<dyn std::error::Error>> {
    match env::var(key) {
        Ok(value) => Ok(value.parse()?),
        Err(_) => Ok(default),
    }
}

fn parse_state_path(key: &str, default: &str) -> Option<std::path::PathBuf> {
    let value = env::var(key).unwrap_or_else(|_| default.to_string());
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(std::path::PathBuf::from(trimmed))
    }
}

fn wants_version_flag() -> bool {
    env::args()
        .skip(1)
        .any(|arg| matches!(arg.as_str(), "-V" | "--version"))
}

fn binary_version_text() -> String {
    let binary = env!("CARGO_PKG_NAME");
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
    use std::sync::{Mutex, OnceLock};

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn parse_addr_uses_default_when_env_missing() {
        let _guard = env_lock().lock().expect("env lock");
        let key = "PD_TEST_CONTROLLER_ADDR_MISSING";
        // SAFETY: tests serialize process env access via env_lock.
        unsafe {
            env::remove_var(key);
        }

        let parsed = parse_addr(key, "127.0.0.1:9910").expect("addr should parse");
        assert_eq!(
            parsed,
            "127.0.0.1:9910"
                .parse::<SocketAddr>()
                .expect("valid literal")
        );
    }

    #[test]
    fn parse_addr_reads_env_when_present() {
        let _guard = env_lock().lock().expect("env lock");
        let key = "PD_TEST_CONTROLLER_ADDR_PRESENT";
        // SAFETY: tests serialize process env access via env_lock.
        unsafe {
            env::set_var(key, "127.0.0.1:9911");
        }

        let parsed = parse_addr(key, "127.0.0.1:9910").expect("addr should parse");
        assert_eq!(
            parsed,
            "127.0.0.1:9911"
                .parse::<SocketAddr>()
                .expect("valid literal")
        );
        // SAFETY: tests serialize process env access via env_lock.
        unsafe {
            env::remove_var(key);
        }
    }

    #[test]
    fn parse_u64_and_usize_use_defaults_when_env_missing() {
        let _guard = env_lock().lock().expect("env lock");
        let u64_key = "PD_TEST_CONTROLLER_U64_MISSING";
        let usize_key = "PD_TEST_CONTROLLER_USIZE_MISSING";
        // SAFETY: tests serialize process env access via env_lock.
        unsafe {
            env::remove_var(u64_key);
            env::remove_var(usize_key);
        }

        assert_eq!(parse_u64(u64_key, 123).expect("u64 parse"), 123);
        assert_eq!(parse_usize(usize_key, 456).expect("usize parse"), 456);
    }

    #[test]
    fn parse_u64_and_usize_read_env_when_present() {
        let _guard = env_lock().lock().expect("env lock");
        let u64_key = "PD_TEST_CONTROLLER_U64_PRESENT";
        let usize_key = "PD_TEST_CONTROLLER_USIZE_PRESENT";
        // SAFETY: tests serialize process env access via env_lock.
        unsafe {
            env::set_var(u64_key, "789");
            env::set_var(usize_key, "321");
        }

        assert_eq!(parse_u64(u64_key, 123).expect("u64 parse"), 789);
        assert_eq!(parse_usize(usize_key, 456).expect("usize parse"), 321);
        // SAFETY: tests serialize process env access via env_lock.
        unsafe {
            env::remove_var(u64_key);
            env::remove_var(usize_key);
        }
    }

    #[test]
    fn parse_state_path_returns_none_for_blank_value() {
        let _guard = env_lock().lock().expect("env lock");
        let key = "PD_TEST_CONTROLLER_STATE_PATH_BLANK";
        // SAFETY: tests serialize process env access via env_lock.
        unsafe {
            env::set_var(key, "   ");
        }

        assert!(parse_state_path(key, ".pd-controller/state.json").is_none());
        // SAFETY: tests serialize process env access via env_lock.
        unsafe {
            env::remove_var(key);
        }
    }

    #[test]
    fn parse_state_path_uses_default_when_env_missing() {
        let _guard = env_lock().lock().expect("env lock");
        let key = "PD_TEST_CONTROLLER_STATE_PATH_MISSING";
        // SAFETY: tests serialize process env access via env_lock.
        unsafe {
            env::remove_var(key);
        }

        let parsed = parse_state_path(key, ".pd-controller/state.json");
        assert_eq!(
            parsed,
            Some(std::path::PathBuf::from(".pd-controller/state.json"))
        );
    }

    #[test]
    fn binary_version_text_contains_binary_name() {
        let text = binary_version_text();
        assert!(text.starts_with(env!("CARGO_PKG_NAME")));
    }
}
