#[path = "../../../tests/support/mod.rs"]
mod support;

#[test]
fn nob_configuration_checks_are_offline_and_reject_spotibot_fallback() {
    support::check_host(env!("CARGO_BIN_EXE_nob"), true);
}
