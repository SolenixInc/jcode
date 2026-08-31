//! Permission setup: read-only checks plus deep-linking and polling for the
//! macOS TCC grants needed for desktop control.

use accessibility_sys::AXIsProcessTrusted;
use anyhow::Result;
use core_graphics::access::ScreenCaptureAccess;
use jcode_tool_types::ToolOutput;
use serde_json::json;
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, Instant};

fn accessibility_ok() -> bool {
    // This is a native, read-only TCC preflight. It does not prompt or act on
    // any application.
    unsafe { AXIsProcessTrusted() }
}

fn screen_recording_ok() -> bool {
    // Preflight only: unlike `request`, this does not prompt and does not
    // capture the screen.
    ScreenCaptureAccess.preflight()
}

fn yes_no(b: bool) -> &'static str {
    if b { "granted" } else { "NOT granted" }
}

/// Format a permission report without consulting TCC or touching the desktop.
pub(super) fn format_permission_report(ax: bool, screen: bool, swift: bool) -> String {
    let mut lines = vec![
        format!("Accessibility (input + AX control): {}", yes_no(ax)),
        format!("Screen Recording (screenshots/OCR): {}", yes_no(screen)),
        format!(
            "Swift toolchain (for OCR):          {}",
            if swift { "present" } else { "missing" }
        ),
    ];
    if !ax || !screen {
        lines.push("Run action='setup' to request these and open the right settings pane.".into());
    }
    lines.join("\n")
}

/// Report status only. This performs native, read-only preflight checks and
/// never prompts, captures the screen, opens or focuses an app, or creates a
/// temporary file.
pub fn check_permissions() -> Result<ToolOutput> {
    let ax = accessibility_ok();
    let screen = screen_recording_ok();
    let swift = std::path::Path::new("/usr/bin/swift").exists()
        || Command::new("/usr/bin/which")
            .arg("swift")
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

    Ok(
        ToolOutput::new(format_permission_report(ax, screen, swift)).with_metadata(json!({
            "accessibility": ax, "screen_recording": screen, "swift": swift,
        })),
    )
}

/// Open the relevant settings panes and poll Accessibility until granted.
pub fn setup() -> Result<ToolOutput> {
    let mut log = Vec::new();

    let ax0 = accessibility_ok();
    let screen0 = screen_recording_ok();
    log.push(format!(
        "Initial: accessibility={}, screen_recording={}",
        ax0, screen0
    ));

    // Setup is the explicitly mutating path: it may open System Settings and
    // wait for the user to change TCC state. The checks above remain preflight
    // only, so setup does not need to capture the screen to detect status.
    if !ax0 {
        // Deep-link to the exact Accessibility pane.
        let _ = Command::new("/usr/bin/open")
            .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
            .status();
        log.push(
            "Opened Privacy & Security > Accessibility. Add and enable your terminal/jcode there."
                .into(),
        );
    }
    if !screen0 {
        let _ = Command::new("/usr/bin/open")
            .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture")
            .status();
        log.push(
            "Opened Privacy & Security > Screen Recording. Add and enable your terminal/jcode there."
                .into(),
        );
    }

    // Poll Accessibility for up to ~30s so the agent can report "ready".
    if !ax0 {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut granted = false;
        while Instant::now() < deadline {
            if accessibility_ok() {
                granted = true;
                break;
            }
            sleep(Duration::from_millis(1000));
        }
        log.push(format!(
            "Accessibility after wait: {}",
            if granted {
                "granted"
            } else {
                "still not granted (toggle it, then re-run check_permissions)"
            }
        ));
    }

    let ax = accessibility_ok();
    let screen = screen_recording_ok();
    log.push(format!(
        "Final: accessibility={}, screen_recording={}",
        ax, screen
    ));
    if !ax {
        log.push(
            "NOTE: the Accessibility toggle cannot be enabled programmatically (macOS security). \
             It is the one switch you must flip by hand."
                .into(),
        );
    }

    Ok(ToolOutput::new(log.join("\n")).with_metadata(json!({
        "accessibility": ax, "screen_recording": screen,
    })))
}
