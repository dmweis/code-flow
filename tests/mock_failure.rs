//! Mock PR: a deliberately failing test to exercise the "CI failed" status.

#[test]
fn deliberately_fails() {
    assert_eq!(1 + 1, 3, "this mock PR is supposed to fail CI");
}
