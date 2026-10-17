//! Replica-local application preferences (never synchronized as notebook content).
//!
//! Side-button action is a stable enum. Malformed, unknown, or missing files fail closed to
//! [`SideButtonAction::NavigationUndo`] (the shipped default).

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::sync_local::DEFAULT_REPLICA_STATE_DIR;

pub const PREFERENCES_FILE: &str = "preferences.json";
const PREFERENCES_VERSION: u32 = 1;

/// What a BTN_STYLUS + Pen Down contact does on a notebook page.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SideButtonAction {
    /// Existing F2/F4 zone nav + center undo. Default for new and malformed installs.
    #[default]
    NavigationUndo,
    /// Capture the tap point and open the link-target picker.
    CreateLink,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct PreferencesFile {
    v: u32,
    side_button: SideButtonAction,
}

impl Default for PreferencesFile {
    fn default() -> Self {
        Self {
            v: PREFERENCES_VERSION,
            side_button: SideButtonAction::NavigationUndo,
        }
    }
}

/// Path for the replica-local preferences file.
///
/// `{sync-dir}/preferences.json` when outbound sync has a state dir, else
/// `{device-dir}/preferences.json` in product mode, else the Kindle app state directory.
pub fn preferences_path(sync_dir: Option<&Path>, device_dir: Option<&Path>) -> PathBuf {
    if let Some(dir) = sync_dir {
        return dir.join(PREFERENCES_FILE);
    }
    if let Some(dir) = device_dir {
        return dir.join(PREFERENCES_FILE);
    }
    PathBuf::from(DEFAULT_REPLICA_STATE_DIR).join(PREFERENCES_FILE)
}

/// Load the side-button action. Missing/unreadable/malformed → default.
pub fn load_side_button(path: &Path) -> SideButtonAction {
    load(path).side_button
}

fn load(path: &Path) -> PreferencesFile {
    let Ok(raw) = fs::read_to_string(path) else {
        return PreferencesFile::default();
    };
    parse_bytes(raw.as_bytes())
}

fn parse_bytes(raw: &[u8]) -> PreferencesFile {
    let Ok(file) = serde_json::from_slice::<PreferencesFile>(raw) else {
        return PreferencesFile::default();
    };
    if file.v != PREFERENCES_VERSION {
        return PreferencesFile::default();
    }
    file
}

/// Persist `side_button`.
pub fn save_side_button(path: &Path, action: SideButtonAction) -> Result<(), String> {
    let file = PreferencesFile {
        v: PREFERENCES_VERSION,
        side_button: action,
    };
    let bytes = serde_json::to_vec_pretty(&file).map_err(|e| e.to_string())?;
    let dir = path.parent().unwrap_or(path);
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let tmp = dir.join(format!("{PREFERENCES_FILE}.tmp"));
    {
        let mut f = File::create(&tmp).map_err(|e| e.to_string())?;
        f.write_all(&bytes).map_err(|e| e.to_string())?;
        f.sync_all().map_err(|e| e.to_string())?;
    }
    #[cfg(windows)]
    if path.exists() {
        fs::remove_file(path).map_err(|e| e.to_string())?;
    }
    fs::rename(&tmp, path).map_err(|e| e.to_string())?;
    Ok(())
}

/// Result of trying to make `next` the durable side-button action.
///
/// The active action changes only when the file write succeeds. A failure keeps
/// `previous` and carries the I/O error for the Settings error dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SideButtonUpdate {
    pub action: SideButtonAction,
    pub error: Option<String>,
}

pub fn apply_side_button(
    previous: SideButtonAction,
    path: &Path,
    next: SideButtonAction,
) -> SideButtonUpdate {
    match save_side_button(path, next) {
        Ok(()) => SideButtonUpdate {
            action: next,
            error: None,
        },
        Err(e) => SideButtonUpdate {
            action: previous,
            error: Some(e),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("scribe-prefs-{}-{}", std::process::id(), name));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.join(PREFERENCES_FILE)
    }

    #[test]
    fn missing_file_is_navigation_undo() {
        let p = temp_path("missing");
        assert_eq!(load_side_button(&p), SideButtonAction::NavigationUndo);
    }

    #[test]
    fn persist_round_trip_and_restore_default() {
        let p = temp_path("round");
        save_side_button(&p, SideButtonAction::CreateLink).unwrap();
        assert_eq!(load_side_button(&p), SideButtonAction::CreateLink);
        save_side_button(&p, SideButtonAction::NavigationUndo).unwrap();
        assert_eq!(load_side_button(&p), SideButtonAction::NavigationUndo);
        let _ = fs::remove_file(&p);
        let _ = fs::remove_dir(p.parent().unwrap());
    }

    #[test]
    fn malformed_and_unknown_fail_to_default() {
        assert_eq!(
            parse_bytes(b"not json").side_button,
            SideButtonAction::NavigationUndo
        );
        assert_eq!(
            parse_bytes(br#"{"v":1,"side_button":"teleport"}"#).side_button,
            SideButtonAction::NavigationUndo
        );
        assert_eq!(
            parse_bytes(br#"{"v":99,"side_button":"create_link"}"#).side_button,
            SideButtonAction::NavigationUndo
        );
        assert_eq!(
            parse_bytes(br#"{"v":1}"#).side_button,
            SideButtonAction::NavigationUndo
        );
    }

    #[test]
    fn persistence_failure_keeps_the_previous_action() {
        let dir =
            std::env::temp_dir().join(format!("scribe-prefs-{}-{}", std::process::id(), "fail"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let good = dir.join(PREFERENCES_FILE);
        save_side_button(&good, SideButtonAction::CreateLink).unwrap();
        let blocker = dir.join("not-a-directory");
        fs::write(&blocker, b"x").unwrap();
        let bad = blocker.join(PREFERENCES_FILE);
        let update = apply_side_button(
            SideButtonAction::CreateLink,
            &bad,
            SideButtonAction::NavigationUndo,
        );
        assert_eq!(update.action, SideButtonAction::CreateLink);
        assert!(update.error.is_some());
        assert_eq!(load_side_button(&good), SideButtonAction::CreateLink);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn path_prefers_sync_dir_then_device_dir() {
        let sync = Path::new("/tmp/sync");
        let device = Path::new("/tmp/device");
        assert_eq!(
            preferences_path(Some(sync), Some(device)),
            sync.join(PREFERENCES_FILE)
        );
        assert_eq!(
            preferences_path(None, Some(device)),
            device.join(PREFERENCES_FILE)
        );
        assert_eq!(
            preferences_path(None, None),
            PathBuf::from(DEFAULT_REPLICA_STATE_DIR).join(PREFERENCES_FILE)
        );
    }
}
