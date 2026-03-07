//! Native shutdown notification so the event loop can exit cleanly.
//!
//! - **Windows**: A channel is created and a console ctrl handler is installed
//!   (close window, Ctrl+C, Ctrl+Break). When one of these occurs, the handler
//!   sends on the channel and the event loop exits.
//! - **Unix (Linux, macOS, BSD)**: No channel; shutdown is driven by the
//!   signal stream in the event loop. We listen for SIGTERM, SIGINT (Ctrl+C),
//!   and SIGHUP (terminal closed, e.g. close tab in macOS Terminal.app or
//!   SSH disconnect). When any of these are received, the event loop exits.

use std::sync::OnceLock;
use tokio::sync::mpsc;

/// Channel sender used by the native handler to request shutdown. Set during
/// [`setup`]; the handler sends one message then the process should exit.
#[cfg(windows)]
static SENDER: OnceLock<mpsc::UnboundedSender<()>> = OnceLock::new();

/// Creates the shutdown channel and registers the native OS handler. Returns
/// the receiver; when it yields, the application should exit the event loop
/// and run cleanup.
///
/// - **Windows**: installs a console ctrl handler (close window, Ctrl+C, etc.)
///   and sends on the channel so the process can exit instead of being killed.
/// - **Unix (Linux, macOS, BSD)**: returns `None`; shutdown is driven by the
///   signal stream (SIGTERM, SIGINT, SIGHUP) in the event loop.
pub fn setup() -> Option<mpsc::UnboundedReceiver<()>> {
    #[cfg(windows)]
    {
        let (tx, rx) = mpsc::unbounded_channel();
        if SENDER.set(tx).is_err() {
            return None;
        }
        if !register_console_ctrl_handler() {
            return None;
        }
        Some(rx)
    }

    #[cfg(not(windows))]
    None
}

#[cfg(windows)]
unsafe extern "system" fn ctrl_handler(ctrl_type: u32) -> i32 {
    use windows_sys::Win32::Foundation::TRUE;
    use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_CLOSE_EVENT, CTRL_C_EVENT};

    let handled = matches!(
        ctrl_type,
        CTRL_C_EVENT | CTRL_BREAK_EVENT | CTRL_CLOSE_EVENT
    );
    if handled {
        if let Some(tx) = SENDER.get() {
            let _ = tx.send(());
        }
    }
    TRUE
}

#[cfg(windows)]
fn register_console_ctrl_handler() -> bool {
    use windows_sys::Win32::Foundation::TRUE;
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;

    let ok = unsafe { SetConsoleCtrlHandler(Some(ctrl_handler), TRUE) };
    if ok == 0 {
        log::warn!("SetConsoleCtrlHandler failed");
        return false;
    }
    true
}
