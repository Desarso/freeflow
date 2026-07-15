use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use arboard::{Clipboard, ImageData};
use async_trait::async_trait;
use freeflow_core::{
    AppContext, FreeFlowError, InjectionResult, Result, SessionType, TextInjector,
};
use tracing::warn;
use x11rb::{
    connection::Connection,
    protocol::{
        xproto::{KEY_PRESS_EVENT, KEY_RELEASE_EVENT},
        xtest::ConnectionExt as _,
    },
};

use crate::{detect_session_type, x11};

const PASTE_SETTLE_DELAY: std::time::Duration = std::time::Duration::from_millis(25);
const FOCUS_SETTLE_DELAY: std::time::Duration = std::time::Duration::from_millis(25);
const CLIPBOARD_RESTORE_DELAY: std::time::Duration = std::time::Duration::from_secs(1);
const CLIPBOARD_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(800);
const MAX_CLIPBOARD_TEXT_BYTES: usize = 8 * 1024 * 1024;
const MAX_CLIPBOARD_IMAGE_BYTES: usize = 64 * 1024 * 1024;
const MAX_CLIPBOARD_FILES: usize = 1_024;

#[derive(Debug, Clone)]
enum ClipboardSnapshot {
    Files(Vec<PathBuf>),
    Image(ImageData<'static>),
    Html {
        html: String,
        alt_text: Option<String>,
    },
    Text(String),
}

struct PendingClipboardRestore {
    id: u64,
    transcript: String,
    snapshot: ClipboardSnapshot,
}

#[derive(Default)]
struct ClipboardState {
    clipboard: Option<Clipboard>,
    pending_restore: Option<PendingClipboardRestore>,
    next_restore_id: u64,
}

trait ClipboardBackend {
    fn get_text(&mut self) -> std::result::Result<String, arboard::Error>;
    fn get_html(&mut self) -> std::result::Result<String, arboard::Error>;
    fn get_file_list(&mut self) -> std::result::Result<Vec<PathBuf>, arboard::Error>;
    fn get_image(&mut self) -> std::result::Result<ImageData<'static>, arboard::Error>;
    fn set_text(&mut self, text: String) -> std::result::Result<(), arboard::Error>;
    fn set_html(
        &mut self,
        html: String,
        alt_text: Option<String>,
    ) -> std::result::Result<(), arboard::Error>;
    fn set_file_list(&mut self, files: &[PathBuf]) -> std::result::Result<(), arboard::Error>;
    fn set_image(&mut self, image: ImageData<'static>) -> std::result::Result<(), arboard::Error>;
}

impl ClipboardBackend for Clipboard {
    fn get_text(&mut self) -> std::result::Result<String, arboard::Error> {
        Clipboard::get_text(self)
    }

    fn get_html(&mut self) -> std::result::Result<String, arboard::Error> {
        self.get().html()
    }

    fn get_file_list(&mut self) -> std::result::Result<Vec<PathBuf>, arboard::Error> {
        self.get().file_list()
    }

    fn get_image(&mut self) -> std::result::Result<ImageData<'static>, arboard::Error> {
        Clipboard::get_image(self)
    }

    fn set_text(&mut self, text: String) -> std::result::Result<(), arboard::Error> {
        Clipboard::set_text(self, text)
    }

    fn set_html(
        &mut self,
        html: String,
        alt_text: Option<String>,
    ) -> std::result::Result<(), arboard::Error> {
        Clipboard::set_html(self, html, alt_text)
    }

    fn set_file_list(&mut self, files: &[PathBuf]) -> std::result::Result<(), arboard::Error> {
        self.set().file_list(files)
    }

    fn set_image(&mut self, image: ImageData<'static>) -> std::result::Result<(), arboard::Error> {
        Clipboard::set_image(self, image)
    }
}

impl ClipboardSnapshot {
    fn capture(clipboard: &mut impl ClipboardBackend) -> Option<Self> {
        if let Ok(files) = clipboard.get_file_list()
            && !files.is_empty()
            && files.len() <= MAX_CLIPBOARD_FILES
        {
            return Some(Self::Files(files));
        }
        if let Ok(image) = clipboard.get_image()
            && image.bytes.len() <= MAX_CLIPBOARD_IMAGE_BYTES
        {
            return Some(Self::Image(image));
        }
        if let Ok(html) = clipboard.get_html()
            && html.len() <= MAX_CLIPBOARD_TEXT_BYTES
        {
            let alt_text = clipboard
                .get_text()
                .ok()
                .filter(|text| text.len() <= MAX_CLIPBOARD_TEXT_BYTES);
            return Some(Self::Html { html, alt_text });
        }
        clipboard
            .get_text()
            .ok()
            .filter(|text| text.len() <= MAX_CLIPBOARD_TEXT_BYTES)
            .map(Self::Text)
    }

    fn restore(
        self,
        clipboard: &mut impl ClipboardBackend,
    ) -> std::result::Result<(), arboard::Error> {
        match self {
            Self::Files(files) => clipboard.set_file_list(&files),
            Self::Image(image) => clipboard.set_image(image),
            Self::Html { html, alt_text } => clipboard.set_html(html, alt_text),
            Self::Text(text) => clipboard.set_text(text),
        }
    }
}

fn restore_if_unchanged(
    clipboard: &mut impl ClipboardBackend,
    transcript: &str,
    snapshot: ClipboardSnapshot,
) -> std::result::Result<bool, arboard::Error> {
    if clipboard.get_text().ok().as_deref() != Some(transcript) {
        return Ok(false);
    }
    snapshot.restore(clipboard)?;
    Ok(true)
}

fn snapshot_for_staging(
    clipboard: &mut impl ClipboardBackend,
    pending: Option<PendingClipboardRestore>,
) -> Option<ClipboardSnapshot> {
    if let Some(pending) = pending
        && clipboard.get_text().ok().as_deref() == Some(&pending.transcript)
    {
        return Some(pending.snapshot);
    }
    ClipboardSnapshot::capture(clipboard)
}

fn ensure_clipboard(state: &mut ClipboardState) -> Result<&mut Clipboard> {
    if state.clipboard.is_none() {
        state.clipboard = Some(Clipboard::new().map_err(|error| {
            FreeFlowError::Clipboard(format!(
                "could not connect to the desktop clipboard: {error}"
            ))
        })?);
    }
    Ok(state.clipboard.as_mut().expect("clipboard was initialized"))
}

#[derive(Clone)]
pub struct LinuxTextInjector {
    clipboard: Arc<Mutex<ClipboardState>>,
}

impl LinuxTextInjector {
    #[must_use]
    pub fn new() -> Self {
        Self {
            clipboard: Arc::new(Mutex::new(ClipboardState::default())),
        }
    }

    async fn set_clipboard(&self, text: &str) -> Result<()> {
        let clipboard = self.clipboard.clone();
        let text = text.to_owned();
        tokio::task::spawn_blocking(move || {
            let mut state = clipboard
                .lock()
                .map_err(|_| FreeFlowError::Clipboard("clipboard lock was poisoned".into()))?;
            state.pending_restore = None;
            ensure_clipboard(&mut state)?
                .set_text(text)
                .map_err(|error| FreeFlowError::Clipboard(error.to_string()))
        })
        .await
        .map_err(|error| FreeFlowError::Internal(error.to_string()))?
    }

    async fn stage_clipboard(&self, text: &str) -> Result<Option<u64>> {
        let clipboard = self.clipboard.clone();
        let text = text.to_owned();
        tokio::task::spawn_blocking(move || {
            let mut state = clipboard
                .lock()
                .map_err(|_| FreeFlowError::Clipboard("clipboard lock was poisoned".into()))?;
            let pending = state.pending_restore.take();
            let snapshot;
            {
                let clipboard = ensure_clipboard(&mut state)?;
                snapshot = snapshot_for_staging(clipboard, pending);
                clipboard
                    .set_text(text.clone())
                    .map_err(|error| FreeFlowError::Clipboard(error.to_string()))?;
            }
            let Some(snapshot) = snapshot else {
                return Ok(None);
            };
            state.next_restore_id = state.next_restore_id.wrapping_add(1).max(1);
            let restore_id = state.next_restore_id;
            state.pending_restore = Some(PendingClipboardRestore {
                id: restore_id,
                transcript: text,
                snapshot,
            });
            Ok(Some(restore_id))
        })
        .await
        .map_err(|error| FreeFlowError::Internal(error.to_string()))?
    }

    async fn restore_clipboard(&self, restore_id: u64) -> Result<bool> {
        tokio::time::sleep(CLIPBOARD_RESTORE_DELAY).await;
        let clipboard = self.clipboard.clone();
        tokio::task::spawn_blocking(move || {
            let mut state = clipboard
                .lock()
                .map_err(|_| FreeFlowError::Clipboard("clipboard lock was poisoned".into()))?;
            if state.pending_restore.as_ref().map(|pending| pending.id) != Some(restore_id) {
                return Ok(false);
            }
            let pending = state
                .pending_restore
                .take()
                .expect("matching clipboard restore was present");
            let clipboard = state.clipboard.as_mut().ok_or_else(|| {
                FreeFlowError::Clipboard("clipboard connection was unavailable".into())
            })?;
            restore_if_unchanged(clipboard, &pending.transcript, pending.snapshot)
                .map_err(|error| FreeFlowError::Clipboard(error.to_string()))
        })
        .await
        .map_err(|error| FreeFlowError::Internal(error.to_string()))?
    }

    fn schedule_clipboard_restore(&self, restore_id: u64) {
        let injector = self.clone();
        tokio::spawn(async move {
            if let Err(error) = injector.restore_clipboard(restore_id).await {
                warn!(
                    category = error.category(),
                    "could not restore the previous clipboard after paste"
                );
            }
        });
    }

    async fn discard_clipboard_restore(&self, restore_id: u64) {
        let clipboard = self.clipboard.clone();
        let _ = tokio::task::spawn_blocking(move || {
            let Ok(mut state) = clipboard.lock() else {
                return;
            };
            if state.pending_restore.as_ref().map(|pending| pending.id) == Some(restore_id) {
                state.pending_restore = None;
            }
        })
        .await;
    }

    fn paste_x11(use_terminal_shortcut: bool) -> Result<()> {
        let (connection, screen) = x11::connect()?;
        let root = connection.setup().roots[screen].root;
        let control = x11::keycode_for_keysym(&connection, xkeysym::key::Control_L)
            .ok_or_else(|| FreeFlowError::Injection("X11 keymap has no Control key".into()))?;
        let shift = x11::keycode_for_keysym(&connection, xkeysym::key::Shift_L)
            .ok_or_else(|| FreeFlowError::Injection("X11 keymap has no Shift key".into()))?;
        let v = x11::keycode_for_keysym(&connection, xkeysym::key::v)
            .ok_or_else(|| FreeFlowError::Injection("X11 keymap has no V key".into()))?;
        let mut keys = vec![control];
        if use_terminal_shortcut {
            keys.push(shift);
        }
        keys.push(v);
        for key in &keys {
            connection
                .xtest_fake_input(KEY_PRESS_EVENT, *key, 0, root, 0, 0, 0)
                .map_err(injection_error)?;
        }
        for key in keys.iter().rev() {
            connection
                .xtest_fake_input(KEY_RELEASE_EVENT, *key, 0, root, 0, 0, 0)
                .map_err(injection_error)?;
        }
        connection.flush().map_err(injection_error)?;
        Ok(())
    }

    async fn paste_wayland(
        use_terminal_shortcut: bool,
        target_window_id: Option<u64>,
    ) -> Result<()> {
        if is_hyprland() {
            if let Some(target_window_id) = target_window_id {
                let target = format!("address:0x{target_window_id:x}");
                let output = tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    tokio::process::Command::new("hyprctl")
                        .args(["dispatch", "focuswindow"])
                        .arg(target)
                        .output(),
                )
                .await
                .map_err(|_| {
                    FreeFlowError::Injection("restoring the target window timed out".into())
                })?
                .map_err(|error| {
                    FreeFlowError::Injection(format!(
                        "could not restore the dictation target: {error}"
                    ))
                })?;
                if !output.status.success() {
                    return Err(FreeFlowError::Injection(
                        "Hyprland could not restore the dictation target".into(),
                    ));
                }
                tokio::time::sleep(FOCUS_SETTLE_DELAY).await;
            }
            let modifiers = if use_terminal_shortcut {
                "CTRL SHIFT"
            } else {
                "CTRL"
            };
            let shortcut = format!("{modifiers},V,activewindow");
            let output = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                tokio::process::Command::new("hyprctl")
                    .args(["dispatch", "sendshortcut"])
                    .arg(shortcut)
                    .output(),
            )
            .await
            .map_err(|_| FreeFlowError::Injection("Hyprland paste timed out".into()))?
            .map_err(|error| {
                FreeFlowError::Injection(format!(
                    "the Hyprland input dispatcher is unavailable: {error}"
                ))
            })?;
            if output.status.success() {
                return Ok(());
            }
        }

        let output = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            tokio::process::Command::new("wtype")
                .args(wayland_paste_args(use_terminal_shortcut))
                .output(),
        )
        .await
        .map_err(|_| FreeFlowError::Injection("Wayland paste timed out".into()))?
        .map_err(|error| {
            FreeFlowError::Injection(format!(
                "the Wayland virtual-keyboard helper is unavailable: {error}"
            ))
        })?;
        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr);
            return Err(FreeFlowError::Injection(format!(
                "the compositor rejected automatic paste: {}",
                detail.trim().chars().take(240).collect::<String>()
            )));
        }
        Ok(())
    }

    async fn wait_for_wayland_clipboard(text: &str) {
        let deadline = tokio::time::Instant::now() + CLIPBOARD_READY_TIMEOUT;
        while tokio::time::Instant::now() < deadline {
            let output = tokio::time::timeout(
                std::time::Duration::from_millis(120),
                tokio::process::Command::new("wl-paste")
                    .arg("--no-newline")
                    .output(),
            )
            .await;
            match output {
                Ok(Ok(output)) if output.status.success() && output.stdout == text.as_bytes() => {
                    return;
                }
                Ok(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => return,
                _ => tokio::time::sleep(std::time::Duration::from_millis(25)).await,
            }
        }
    }
}

impl Default for LinuxTextInjector {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TextInjector for LinuxTextInjector {
    async fn inject(&self, text: &str, context: &AppContext) -> Result<InjectionResult> {
        let restore_id = self.stage_clipboard(text).await?;
        let session = detect_session_type();
        if session == SessionType::Wayland {
            Self::wait_for_wayland_clipboard(text).await;
        }
        // Realtime finalization normally overlaps the physical key release.
        // Retain one compositor frame of settling for unusually fast fallback
        // responses without adding a long fixed delay after every request.
        tokio::time::sleep(PASTE_SETTLE_DELAY).await;
        let terminal = context.is_terminal;
        let paste = match session {
            SessionType::X11 => tokio::task::spawn_blocking(move || Self::paste_x11(terminal))
                .await
                .map_err(|error| FreeFlowError::Internal(error.to_string()))?,
            SessionType::Wayland => Self::paste_wayland(terminal, context.active_window_id).await,
            SessionType::Unknown => Err(FreeFlowError::Injection(
                "no supported desktop input service is available".into(),
            )),
        };
        match paste {
            Ok(()) => {
                let clipboard_retained = match restore_id {
                    Some(restore_id) => {
                        self.schedule_clipboard_restore(restore_id);
                        false
                    }
                    None => true,
                };
                Ok(InjectionResult {
                    strategy: match (session, terminal) {
                        (SessionType::X11, true) => "x11ClipboardCtrlShiftV".into(),
                        (SessionType::X11, false) => "x11ClipboardCtrlV".into(),
                        (SessionType::Wayland, true) => "waylandClipboardCtrlShiftV".into(),
                        (SessionType::Wayland, false) => "waylandClipboardCtrlV".into(),
                        (SessionType::Unknown, _) => unreachable!("unknown sessions cannot paste"),
                    },
                    pasted: true,
                    clipboard_retained,
                    requires_manual_paste: false,
                    message: None,
                })
            }
            Err(error) => {
                if let Some(restore_id) = restore_id {
                    self.discard_clipboard_restore(restore_id).await;
                }
                Err(FreeFlowError::Injection(format!(
                    "automatic paste failed: {error}. The transcript remains in the clipboard"
                )))
            }
        }
    }

    async fn copy_to_clipboard(&self, text: &str) -> Result<()> {
        self.set_clipboard(text).await
    }
}

fn is_hyprland() -> bool {
    std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .contains("hyprland")
        || std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some()
}

fn wayland_paste_args(use_terminal_shortcut: bool) -> Vec<&'static str> {
    let mut arguments = vec!["-M", "ctrl"];
    if use_terminal_shortcut {
        arguments.extend(["-M", "shift"]);
    }
    arguments.extend(["-k", "v"]);
    if use_terminal_shortcut {
        arguments.extend(["-m", "shift"]);
    }
    arguments.extend(["-m", "ctrl"]);
    arguments
}

fn injection_error(error: impl std::fmt::Display) -> FreeFlowError {
    FreeFlowError::Injection(format!("X11 synthetic paste failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeClipboard {
        text: Option<String>,
        html: Option<String>,
        files: Option<Vec<PathBuf>>,
    }

    impl ClipboardBackend for FakeClipboard {
        fn get_text(&mut self) -> std::result::Result<String, arboard::Error> {
            self.text.clone().ok_or(arboard::Error::ContentNotAvailable)
        }

        fn get_html(&mut self) -> std::result::Result<String, arboard::Error> {
            self.html.clone().ok_or(arboard::Error::ContentNotAvailable)
        }

        fn get_file_list(&mut self) -> std::result::Result<Vec<PathBuf>, arboard::Error> {
            self.files
                .clone()
                .ok_or(arboard::Error::ContentNotAvailable)
        }

        fn get_image(&mut self) -> std::result::Result<ImageData<'static>, arboard::Error> {
            Err(arboard::Error::ContentNotAvailable)
        }

        fn set_text(&mut self, text: String) -> std::result::Result<(), arboard::Error> {
            self.text = Some(text);
            self.html = None;
            self.files = None;
            Ok(())
        }

        fn set_html(
            &mut self,
            html: String,
            alt_text: Option<String>,
        ) -> std::result::Result<(), arboard::Error> {
            self.html = Some(html);
            self.text = alt_text;
            self.files = None;
            Ok(())
        }

        fn set_file_list(&mut self, files: &[PathBuf]) -> std::result::Result<(), arboard::Error> {
            self.files = Some(files.to_vec());
            self.text = None;
            self.html = None;
            Ok(())
        }

        fn set_image(
            &mut self,
            _image: ImageData<'static>,
        ) -> std::result::Result<(), arboard::Error> {
            Ok(())
        }
    }

    #[test]
    fn restores_the_previous_text_after_the_transcript_is_consumed() {
        let mut clipboard = FakeClipboard {
            text: Some("previous clipboard".into()),
            ..FakeClipboard::default()
        };
        let snapshot = ClipboardSnapshot::capture(&mut clipboard).unwrap();
        clipboard.set_text("dictated text".into()).unwrap();

        assert!(restore_if_unchanged(&mut clipboard, "dictated text", snapshot).unwrap());
        assert_eq!(clipboard.text.as_deref(), Some("previous clipboard"));
    }

    #[test]
    fn does_not_overwrite_a_newer_user_clipboard_value() {
        let mut clipboard = FakeClipboard {
            text: Some("previous clipboard".into()),
            ..FakeClipboard::default()
        };
        let snapshot = ClipboardSnapshot::capture(&mut clipboard).unwrap();
        clipboard.set_text("new user copy".into()).unwrap();

        assert!(!restore_if_unchanged(&mut clipboard, "dictated text", snapshot).unwrap());
        assert_eq!(clipboard.text.as_deref(), Some("new user copy"));
    }

    #[test]
    fn preserves_rich_html_with_its_plain_text_fallback() {
        let mut clipboard = FakeClipboard {
            text: Some("formatted text".into()),
            html: Some("<b>formatted</b> text".into()),
            ..FakeClipboard::default()
        };
        let snapshot = ClipboardSnapshot::capture(&mut clipboard).unwrap();
        clipboard.set_text("dictated text".into()).unwrap();

        assert!(restore_if_unchanged(&mut clipboard, "dictated text", snapshot).unwrap());
        assert_eq!(clipboard.html.as_deref(), Some("<b>formatted</b> text"));
        assert_eq!(clipboard.text.as_deref(), Some("formatted text"));
    }

    #[test]
    fn keeps_the_transcript_staged_for_delayed_input_consumers() {
        assert!(
            CLIPBOARD_RESTORE_DELAY >= std::time::Duration::from_secs(1),
            "restoring sooner can make a delayed target paste the previous clipboard"
        );
    }

    #[test]
    fn carries_the_original_clipboard_across_rapid_dictations() {
        let mut clipboard = FakeClipboard {
            text: Some("original clipboard".into()),
            ..FakeClipboard::default()
        };
        let original = ClipboardSnapshot::capture(&mut clipboard).unwrap();
        clipboard.set_text("first transcript".into()).unwrap();

        let snapshot = snapshot_for_staging(
            &mut clipboard,
            Some(PendingClipboardRestore {
                id: 1,
                transcript: "first transcript".into(),
                snapshot: original,
            }),
        )
        .unwrap();
        clipboard.set_text("second transcript".into()).unwrap();

        assert!(restore_if_unchanged(&mut clipboard, "second transcript", snapshot).unwrap());
        assert_eq!(clipboard.text.as_deref(), Some("original clipboard"));
    }

    #[test]
    fn a_newer_user_copy_replaces_a_pending_clipboard_snapshot() {
        let mut clipboard = FakeClipboard {
            text: Some("original clipboard".into()),
            ..FakeClipboard::default()
        };
        let original = ClipboardSnapshot::capture(&mut clipboard).unwrap();
        clipboard.set_text("first transcript".into()).unwrap();
        clipboard.set_text("new user copy".into()).unwrap();

        let snapshot = snapshot_for_staging(
            &mut clipboard,
            Some(PendingClipboardRestore {
                id: 1,
                transcript: "first transcript".into(),
                snapshot: original,
            }),
        )
        .unwrap();
        clipboard.set_text("second transcript".into()).unwrap();

        assert!(restore_if_unchanged(&mut clipboard, "second transcript", snapshot).unwrap());
        assert_eq!(clipboard.text.as_deref(), Some("new user copy"));
    }

    #[test]
    fn wayland_paste_uses_ctrl_v_for_regular_fields() {
        assert_eq!(
            wayland_paste_args(false),
            ["-M", "ctrl", "-k", "v", "-m", "ctrl"]
        );
    }

    #[test]
    fn wayland_paste_uses_ctrl_shift_v_for_terminals() {
        assert_eq!(
            wayland_paste_args(true),
            [
                "-M", "ctrl", "-M", "shift", "-k", "v", "-m", "shift", "-m", "ctrl"
            ]
        );
    }
}
