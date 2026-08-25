use std::ffi::{CStr, CString, c_char};
use std::io::{BufRead, BufReader};
use std::os::unix::io::FromRawFd;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use pumpkin_config::{LoadConfiguration, PumpkinConfig};

use crate::crash::{CrashReport, FullBacktrace};
use crate::log_ring::LogRing;
use crate::server::Server;
use crate::{PumpkinServer, data::VanillaData, init_logger, stop_server};

static LOG_BUFFER: LazyLock<Mutex<LogRing>> = LazyLock::new(|| Mutex::new(LogRing::new()));
static LOG_CAPTURE_STARTED: AtomicBool = AtomicBool::new(false);

/// Handle to the currently running server, if any, so the C API can answer
/// stats/player queries while `pumpkin_start` is blocked inside the runtime.
static ACTIVE_SERVER: Mutex<Option<Arc<Server>>> = Mutex::new(None);
/// Configured java max_players, captured at startup (the config is moved into
/// the server and not reachable afterwards).
static MAX_PLAYERS: AtomicU32 = AtomicU32::new(0);

fn active_server() -> Option<Arc<Server>> {
    ACTIVE_SERVER.lock().unwrap().clone()
}

/// Append a line to the log buffer directly — reliable even when stderr
/// redirection hasn't started or the process is about to die.
fn buffer_log_line(line: String) {
    LOG_BUFFER.lock().unwrap().push(line);
}

// Declare just the three POSIX calls we need rather than pulling in the libc crate.
unsafe extern "C" {
    fn pipe(fds: *mut i32) -> i32;
    fn dup2(oldfd: i32, newfd: i32) -> i32;
    fn close(fd: i32) -> i32;
}

/// Redirect stdout + stderr into an in-memory line buffer so the Swift UI can
/// display Pumpkin's tracing output (which would otherwise go to /dev/null on iOS).
fn start_log_capture() {
    if LOG_CAPTURE_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }

    let mut fds = [0i32; 2]; // [read_end, write_end]
    unsafe {
        pipe(fds.as_mut_ptr());
        dup2(fds[1], 1); // stdout → write-end
        dup2(fds[1], 2); // stderr → write-end
        close(fds[1]); // fd 1 & 2 now hold the write-end; close the extra ref
    }

    let read_fd = fds[0];
    std::thread::spawn(move || {
        let file = unsafe { std::fs::File::from_raw_fd(read_fd) };
        for line in BufReader::new(file).lines() {
            let Ok(l) = line else { break };
            let trimmed = l.trim().to_owned();
            if trimmed.is_empty() {
                continue;
            }
            buffer_log_line(trimmed);
        }
    });
}

/// On the binary target `main.rs` installs a panic hook that writes
/// `crash-reports/`; nothing does that on the iOS FFI path, so a panic aborts
/// the whole app with its message lost down the redirected stderr. Install a
/// lean hook that persists the report into the active data dir (the CWD)
/// before the abort.
fn install_panic_hook() {
    static INSTALLED: AtomicBool = AtomicBool::new(false);
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    std::panic::set_hook(Box::new(|panic_info| {
        use std::backtrace::{Backtrace, BacktraceStatus};
        use std::io::Write;

        // Absolute path first: the panic may fire before the CWD is switched
        // to the data dir, and ./crash-reports would then be unwritable.
        let fallback = std::env::temp_dir().join("pumpkin-panic.log");
        if let Ok(mut file) = std::fs::File::create(&fallback) {
            let _ = writeln!(file, "{panic_info}");
            let _ = writeln!(file, "{}", Backtrace::force_capture());
            let _ = file.sync_all();
        }

        let captured = Backtrace::capture();
        let full = if captured.status() == BacktraceStatus::Captured {
            FullBacktrace::Captured
        } else {
            FullBacktrace::ForceCaptured(Backtrace::force_capture())
        };
        let report = CrashReport::new(panic_info, captured, full);
        // Straight into the log buffer, so a live log view shows at least the
        // location before the process dies.
        buffer_log_line(format!("PANIC: {panic_info}"));
        let _ = report.save();
    }));
}

// ── C API ────────────────────────────────────────────────────────────────────

/// Total number of log lines buffered so far.
#[unsafe(no_mangle)]
pub extern "C" fn pumpkin_log_count() -> u64 {
    LOG_BUFFER.lock().unwrap().total()
}

/// Return lines `from_index..end` joined by `\n` as a C string.
/// The caller must free the pointer with `pumpkin_free_string`.
#[unsafe(no_mangle)]
pub extern "C" fn pumpkin_logs_since(from_index: u64) -> *mut c_char {
    let joined = LOG_BUFFER.lock().unwrap().since(from_index);
    CString::new(joined).unwrap_or_default().into_raw()
}

/// Free a string returned by `pumpkin_logs_since`.
#[unsafe(no_mangle)]
pub extern "C" fn pumpkin_free_string(ptr: *mut c_char) {
    if !ptr.is_null() {
        unsafe {
            let _ = CString::from_raw(ptr);
        }
    }
}

/// Start the Pumpkin server. Blocks the calling thread until the server stops.
/// `data_dir` must be a null-terminated UTF-8 path to a writable directory.
/// Call from a background thread in your iOS app.
#[unsafe(no_mangle)]
pub extern "C" fn pumpkin_start(data_dir: *const c_char) {
    // Reset statics so the server can start again after a previous run.
    crate::reset_server_state();
    start_log_capture();
    install_panic_hook();

    let path = unsafe { CStr::from_ptr(data_dir) }
        .to_str()
        .expect("pumpkin_start: data_dir is not valid UTF-8");

    std::env::set_current_dir(path).expect("pumpkin_start: could not switch to data directory");

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("pumpkin_start: failed to build Tokio runtime");

    rt.block_on(async {
        let exec_dir = std::env::current_dir().unwrap();
        let config = PumpkinConfig::load(&exec_dir);
        let vanilla_data = VanillaData::load();
        init_logger(&config.advanced);

        MAX_PLAYERS.store(
            config.advanced.networking.java.max_players,
            Ordering::Relaxed,
        );

        // Pre-flight the bind so a taken port is reported before any of the
        // server's tasks are spawned. `PumpkinServer::new` now returns the
        // error rather than exiting, so this is no longer load-bearing for
        // survival — but it still produces the clearer message, and it closes
        // the window where a half-built server has to be torn down again.
        if config.advanced.networking.java.enabled {
            let address = config.advanced.networking.java.address;
            match std::net::TcpListener::bind(address) {
                Ok(probe) => drop(probe),
                Err(e) => {
                    buffer_log_line(format!(
                        "ERROR Cannot start server: failed to bind {address}: {e}. \
                         Is another server instance already running?"
                    ));
                    return;
                }
            }
        }

        let server = match PumpkinServer::new(config.basic, config.advanced, vanilla_data).await {
            Ok(server) => server,
            Err(e) => {
                buffer_log_line(format!("ERROR Cannot start server: {e}"));
                return;
            }
        };
        *ACTIVE_SERVER.lock().unwrap() = Some(server.server.clone());
        server.init_plugins().await;
        server.start().await;
    });

    *ACTIVE_SERVER.lock().unwrap() = None;
}

/// Signal the server to stop gracefully.
#[unsafe(no_mangle)]
pub extern "C" fn pumpkin_stop() {
    stop_server();
}

/// Live server stats as a JSON object:
/// `{"running":bool,"tps":f64,"mspt":f64,"online":u32,"max":u32}`.
/// TPS/MSPT are averaged over the last 100 ticks, matching the vanilla
/// debug overlay. Free the returned pointer with `pumpkin_free_string`.
#[unsafe(no_mangle)]
pub extern "C" fn pumpkin_stats_json() -> *mut c_char {
    let json = match active_server() {
        None => r#"{"running":false,"tps":0,"mspt":0,"online":0,"max":0}"#.to_owned(),
        Some(server) => {
            let tick_count = server.tick_count.load(Ordering::Relaxed).max(0);
            let samples = i64::from(tick_count.min(100));
            let aggregated = server.aggregated_tick_times_nanos.load(Ordering::Relaxed);
            let (tps, mspt) = if samples > 0 && aggregated > 0 {
                let avg_nanos = aggregated as f64 / samples as f64;
                let target_tps = f64::from(server.tick_rate_manager.tickrate());
                (
                    (1_000_000_000.0 / avg_nanos).min(target_tps),
                    avg_nanos / 1_000_000.0,
                )
            } else {
                (0.0, 0.0)
            };
            format!(
                "{{\"running\":true,\"tps\":{:.1},\"mspt\":{:.2},\"online\":{},\"max\":{}}}",
                tps,
                mspt,
                server.get_player_count(),
                MAX_PLAYERS.load(Ordering::Relaxed),
            )
        }
    };
    CString::new(json).unwrap_or_default().into_raw()
}

/// Connected players as a JSON array of
/// `{"name":string,"uuid":string,"ping":u32}` objects (empty array when the
/// server is not running). Free the returned pointer with `pumpkin_free_string`.
#[unsafe(no_mangle)]
pub extern "C" fn pumpkin_players_json() -> *mut c_char {
    let players = active_server().map_or_else(Vec::new, |server| {
        server
            .get_all_players()
            .iter()
            .map(|player| {
                serde_json::json!({
                    "name": player.gameprofile.name,
                    "uuid": player.gameprofile.id.to_string(),
                    "ping": player.ping.load(Ordering::Relaxed),
                })
            })
            .collect()
    });
    let json = serde_json::Value::Array(players).to_string();
    CString::new(json).unwrap_or_default().into_raw()
}
