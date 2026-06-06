use crate::index::anchors::{self, AnchorStatus};

#[test]
fn exact_anchor_matches() {
    let text = "one\nfn target() {}\nthree\n";
    let anchor = anchors::anchor_for_text("fn target() {}\n", 2, 2, text);
    assert_eq!(anchors::validate("fn target() {}\n", 2, 2, &anchor, text), AnchorStatus::Exact);
}

#[test]
fn small_line_drift_relocates() {
    let original = "a\nb\nc\nfn target() {}\nd\ne\nf\n";
    let current = "inserted\na\nb\nc\nfn target() {}\nd\ne\nf\n";
    let anchor = anchors::anchor_for_text("fn target() {}\n", 4, 4, original);
    let status = anchors::validate("fn target() {}\n", 4, 4, &anchor, current);
    assert!(matches!(status, AnchorStatus::Relocated { start_line: 5, end_line: 5, .. }));
}

#[test]
fn large_rewrite_is_stale() {
    let original = "one\nfn target() {}\nthree\n";
    let current = "completely\ndifferent\nfile\n";
    let anchor = anchors::anchor_for_text("fn target() {}\n", 2, 2, original);
    assert_eq!(anchors::validate("fn target() {}\n", 2, 2, &anchor, current), AnchorStatus::Stale);
}
