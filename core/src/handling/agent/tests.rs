use super::*;
use shishi_acp::client::PermissionOption;

#[test]
fn dsh_permissions_are_allowed_lasting_first() {
    let option = |id: &str, kind: &str| PermissionOption { option_id: id.into(), name: id.into(), kind: kind.into() };
    let options = [option("no", "reject_once"), option("once", "allow_once"), option("always", "allow_always")];
    assert_eq!(allow(&options), Some("always"));
    assert_eq!(allow(&options[..2]), Some("once"));
    assert_eq!(allow(&options[..1]), None);
}
