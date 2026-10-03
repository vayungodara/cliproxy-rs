//! Videos handler tests. The routes are checked against the Go server in
//! tests/media_go.rs; these cover what the goldens cannot reach in one process run.

use super::*;

#[test]
fn routing_keeps_preview_alias() {
    assert_eq!(routing("grok/grok-imagine-video-1.5-preview"), XAI_15_PREVIEW);
    assert_eq!(canonical("grok-imagine-video-1.5-preview"), XAI_15_MODEL);
    assert_eq!(routing("sora-2-pro"), XAI_MODEL);
}

/// `videoAuthBindings`: an expired binding is gone on read, and a write sweeps the
/// expired ones; blank IDs or credentials are never stored.
#[test]
fn bindings_expire() {
    bind("vid-ttl-a", "auth-1", " grok-imagine-video ", Duration::from_secs(60));
    assert_eq!(
        binding(" vid-ttl-a "),
        Some(("auth-1".into(), "grok-imagine-video".into()))
    );
    bind("vid-ttl-b", "auth-2", "m", Duration::ZERO);
    std::thread::sleep(Duration::from_millis(5));
    assert_eq!(binding("vid-ttl-b"), None);
    bind("vid-ttl-c", "auth-3", "m", Duration::ZERO);
    std::thread::sleep(Duration::from_millis(5));
    bind("vid-ttl-d", "auth-4", "m", Duration::from_secs(60));
    assert!(!bindings().contains_key("vid-ttl-c"), "a write sweeps expired entries");
    bind("  ", "auth-5", "m", Duration::from_secs(60));
    bind("vid-ttl-e", " ", "m", Duration::from_secs(60));
    assert_eq!(binding("vid-ttl-e"), None);
}

#[test]
fn seconds_clamp_like_go() {
    assert_eq!(seconds(""), Ok(4));
    assert_eq!(seconds("+20"), Ok(15));
    assert_eq!(seconds("-3"), Ok(1));
    assert_eq!(seconds("1.5"), Err("seconds must be an integer".into()));
    assert_eq!(seconds("9223372036854775808"), Err("seconds must be an integer".into()));
}
