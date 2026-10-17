//! Pure protocol helpers shared by the lifecycle host (no LIPC linkage).
//!
//! Formats proven from KOReaderIntegration `main.c` and SH Integration
//! launcher strings on Kindle FW 5.18.6.

#![cfg_attr(not(unix), allow(dead_code))]

/// Extract the appmgrd request id from a lifecycle payload (`"<id>:<rest>"`).
pub fn request_id_from_payload(payload: &str) -> &str {
    payload.split(':').next().unwrap_or("0")
}

/// Proven acknowledgement value: `"<id>:0:"`.
pub fn ack_value(request_id: &str) -> String {
    format!("{request_id}:0:")
}

/// Proven runresult value: `"0:<service>"` (no trailing colon).
pub fn runresult_value(service: &str) -> String {
    format!("0:{service}")
}

/// Result property name: `load` → `loadresult`, etc.
pub fn result_property(lifecycle_prop: &str) -> String {
    format!("{lifecycle_prop}result")
}

/// F11.1 powerd source and the two proven FW 5.18.6 screensaver events we relay.
pub const POWERD: &str = "com.lab126.powerd";
pub const EVENT_GOING_TO_SCREENSAVER: &str = "goingToScreenSaver";
/// Last powerd event of a wake, observed after blanket has cleared `/dev/fb0` to white.
pub const EVENT_EXITING_SCREENSAVER: &str = "exitingScreenSaver";

/// Kindle home (Library) app. Same proven handoff as `scribe-app`'s Home exit icon: asking
/// appmgrd to start it pauses/unloads `com.markrai.ignite` through the normal lifecycle.
pub const KINDLE_HOME_APP_URI: &str = "app://com.lab126.KPPMainApp";

/// Reserved `scribe-app` exit status: this process lost the singleton and never ran a
/// session. Distinct from `0` (hosted session ended) and `1` (startup/session failure).
/// Must stay in lockstep with `scribe_app::singleton::EXIT_ALREADY_RUNNING`.
pub const CHILD_EXIT_ALREADY_RUNNING: i32 = 79;

/// Whether an *unrequested* child wait status should ask appmgrd to start Kindle Home.
///
/// - `Some(0)` / `Some(1)` / signal (`None`) → yes (session ended or failed).
/// - `Some(CHILD_EXIT_ALREADY_RUNNING)` → no (another live `scribe-app` already owns the panel).
pub fn should_handoff_kindle_home(child_exit_code: Option<i32>) -> bool {
    child_exit_code != Some(CHILD_EXIT_ALREADY_RUNNING)
}

/// Child flag that enables the stdin display-event relay in `scribe-app`.
pub const CHILD_DISPLAY_EVENTS_FLAG: &str = "--display-events-stdin";
/// Relay lines written to the child's stdin (one per event).
pub const RELAY_INVALIDATE: &str = "display-invalidate";
pub const RELAY_RESTORE: &str = "display-restore";

/// Map a powerd event name to the relay line (unknown events are not relayed).
pub fn relay_line_for_powerd_event(event: &str) -> Option<&'static str> {
    match event {
        EVENT_GOING_TO_SCREENSAVER => Some(RELAY_INVALIDATE),
        EVENT_EXITING_SCREENSAVER => Some(RELAY_RESTORE),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ack_and_runresult_formats() {
        assert_eq!(ack_value("0"), "0:0:");
        assert_eq!(ack_value("2"), "2:0:");
        assert_eq!(
            runresult_value("com.markrai.ignite.lifecycleprobe"),
            "0:com.markrai.ignite.lifecycleprobe"
        );
        assert_eq!(result_property("go"), "goresult");
        assert_eq!(result_property("pause"), "pauseresult");
        assert_eq!(result_property("unload"), "unloadresult");
        assert_eq!(result_property("load"), "loadresult");
        assert_eq!(request_id_from_payload("7:app://com.markrai.ignite"), "7");
    }

    #[test]
    fn powerd_screensaver_events_map_to_relay_lines() {
        assert_eq!(
            relay_line_for_powerd_event("goingToScreenSaver"),
            Some("display-invalidate")
        );
        assert_eq!(
            relay_line_for_powerd_event("exitingScreenSaver"),
            Some("display-restore")
        );
        // Earlier wake events precede blanket's white clear; never redraw on them.
        for early in [
            "outOfScreenSaver",
            "wakeupFromSuspend",
            "resuming",
            "readyToSuspend",
        ] {
            assert_eq!(relay_line_for_powerd_event(early), None);
        }
    }

    #[test]
    fn already_running_child_does_not_handoff_home() {
        assert_eq!(CHILD_EXIT_ALREADY_RUNNING, 79);
        assert!(
            !should_handoff_kindle_home(Some(CHILD_EXIT_ALREADY_RUNNING)),
            "singleton loser is a no-op launch"
        );
    }

    #[test]
    fn normal_and_failed_child_exits_still_handoff_home() {
        assert!(
            should_handoff_kindle_home(Some(0)),
            "hosted session ended normally"
        );
        assert!(
            should_handoff_kindle_home(Some(1)),
            "startup/session failure still hands the panel back"
        );
        assert!(
            should_handoff_kindle_home(None),
            "signal/crash still hands the panel back"
        );
        for other in [2, 64, 75, 78, 80, 255] {
            assert!(
                should_handoff_kindle_home(Some(other)),
                "unrelated exit {other} must not be treated as already-running"
            );
        }
    }
}
