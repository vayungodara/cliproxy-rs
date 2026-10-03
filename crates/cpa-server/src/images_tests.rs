//! Images handler tests.

use super::*;

#[test]
fn sniff_png() {
    assert_eq!(detect_content_type(b"\x89PNG\x0D\x0A\x1A\x0Axx"), "image/png");
}
