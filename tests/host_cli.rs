mod support;

#[test]
fn spotibot_configuration_checks_are_offline_and_preserve_default_entrypoint() {
    support::check_host(env!("CARGO_BIN_EXE_discord-spotify-player"), false);
}
