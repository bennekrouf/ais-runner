//! Native OS desktop notifications (Windows toast / macOS Notification Center /
//! Linux libnotify), for events worth surfacing even when the app isn't focused.

use notify_rust::Notification;

fn show(summary: &str, body: &str) {
    if let Err(e) = Notification::new().summary(summary).body(body).show() {
        tracing::warn!("desktop notification failed: {e}");
    }
}

/// Fire-and-forget: runs the (blocking) OS call off the async runtime so a
/// slow notification daemon never stalls the poll loop that triggered it.
fn show_async(summary: String, body: String) {
    tokio::task::spawn_blocking(move || show(&summary, &body));
}

pub fn workflow_succeeded(name: &str, detail: &str) {
    show_async(format!("✅ {name}"), detail.to_string());
}

pub fn workflow_failed(name: &str, detail: &str) {
    show_async(format!("❌ {name} failed"), detail.to_string());
}

pub fn workflow_timed_out(name: &str, detail: &str) {
    show_async(format!("⚠ {name} timed out"), detail.to_string());
}

pub fn emulator_ready(name: &str) {
    show_async(
        format!("{name} ready"),
        "Emulator is up and accepting connections".into(),
    );
}

pub fn emulator_failed(name: &str, detail: &str) {
    show_async(format!("{name} failed to start"), detail.to_string());
}
