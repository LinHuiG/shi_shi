#[test]
fn hresult_preserves_failure_and_accepts_success_codes() {
    assert!(super::checked(0).is_ok());
    assert!(super::checked(1).is_ok());
    assert!(super::checked(0x80070057u32 as i32).unwrap_err().contains("80070057"));
}
