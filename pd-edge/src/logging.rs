use owo_colors::OwoColorize;
use std::sync::OnceLock;
use supports_color::Stream;
use tracing_subscriber::EnvFilter;

static ANSI_ENABLED: OnceLock<bool> = OnceLock::new();

pub fn init(enabled: bool) -> Result<(), Box<dyn std::error::Error>> {
    if !enabled {
        return Ok(());
    }

    let ansi = detect_ansi();
    // let _ = ANSI_ENABLED.set(ansi && false);
    // temporarily disable ansi
    let _ = ANSI_ENABLED.set(false);

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(ansi)
        .with_target(false)
        .compact()
        .try_init()
        .map_err(|err| std::io::Error::other(err.to_string()))?;
    Ok(())
}

pub fn category_access() -> String {
    if ansi_enabled() {
        format!("{}", "ACCESS".bright_cyan().bold())
    } else {
        "ACCESS".to_string()
    }
}

pub fn category_program() -> String {
    if ansi_enabled() {
        format!("{}", "PROGRAM".bright_green().bold())
    } else {
        "PROGRAM".to_string()
    }
}

pub fn category_debug() -> String {
    if ansi_enabled() {
        format!("{}", "DEBUG".bright_magenta().bold())
    } else {
        "DEBUG".to_string()
    }
}

pub fn method_label(method: &str) -> String {
    if !ansi_enabled() {
        return method.to_string();
    }

    match method {
        "GET" => format!("{}", method.bright_blue()),
        "POST" => format!("{}", method.bright_green()),
        "PUT" => format!("{}", method.bright_yellow()),
        "DELETE" => format!("{}", method.bright_red()),
        "PATCH" => format!("{}", method.bright_magenta()),
        _ => format!("{}", method.bright_white()),
    }
}

pub fn status_label(status: u16) -> String {
    let text = status.to_string();
    if !ansi_enabled() {
        return text;
    }

    match status {
        100..=199 => format!("{}", text.bright_blue()),
        200..=299 => format!("{}", text.bright_green()),
        300..=399 => format!("{}", text.bright_cyan()),
        400..=499 => format!("{}", text.bright_yellow()),
        _ => format!("{}", text.bright_red()),
    }
}

fn ansi_enabled() -> bool {
    *ANSI_ENABLED.get_or_init(detect_ansi)
}

fn detect_ansi() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }

    if std::env::var_os("FORCE_COLOR").is_some() {
        let _ = enable_ansi_support();
        return true;
    }

    let windows_vt = enable_ansi_support().is_ok();
    windows_vt || supports_color::on_cached(Stream::Stdout).is_some()
}

#[cfg(windows)]
fn enable_ansi_support() -> windows::core::Result<()> {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Console::{
        ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, GetStdHandle, STD_OUTPUT_HANDLE,
        SetConsoleMode,
    };

    unsafe {
        let handle = GetStdHandle(STD_OUTPUT_HANDLE)?;
        if handle == HANDLE::default() {
            println!("Failed to get console handle");
            return Ok(());
        }

        let mut mode = std::mem::zeroed();
        GetConsoleMode(handle, &mut mode)?;
        SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING)?;
        Ok(())
    }
}

#[cfg(not(windows))]
fn enable_ansi_support() -> Result<(), ()> {
    Err(())
}
