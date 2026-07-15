use arboard::Clipboard;
use freeflow_core::{AppContext, TextInjector};
use freeflow_platform_linux::LinuxTextInjector;

#[tokio::test]
#[ignore = "requires an interactive Wayland session and focused editable field"]
async fn pastes_into_the_focused_wayland_field() {
    let text = std::env::var("FREEFLOW_WAYLAND_TEST_TEXT")
        .expect("set FREEFLOW_WAYLAND_TEST_TEXT to run the interactive injection test");
    let mut clipboard = Clipboard::new().expect("Wayland clipboard should be available");
    let original = clipboard
        .get_text()
        .expect("interactive test requires text in the clipboard");
    let result = LinuxTextInjector::new()
        .inject(&text, &AppContext::default())
        .await
        .expect("Wayland injection should succeed");

    assert!(result.pasted);
    assert!(!result.clipboard_retained);
    assert!(!result.requires_manual_paste);
    assert_eq!(result.strategy, "waylandClipboardCtrlV");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        if clipboard
            .get_text()
            .is_ok_and(|current| current == original)
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the previous clipboard was not restored after delivery"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}
