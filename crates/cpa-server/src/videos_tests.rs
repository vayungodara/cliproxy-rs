//! Videos handler tests.

use super::*;

#[test]
fn routing_keeps_preview_alias() {
    assert_eq!(routing("grok/grok-imagine-video-1.5-preview"), XAI_15_PREVIEW);
    assert_eq!(canonical("grok-imagine-video-1.5-preview"), XAI_15_MODEL);
}
