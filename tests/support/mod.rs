use std::fs;
use std::process::Command;

pub fn check_host(binary: &str, nob: bool) {
    let root = std::env::temp_dir().join(format!(
        "bot-cli-test-{}-{}",
        std::process::id(),
        if nob { "nob" } else { "spotibot" }
    ));
    fs::create_dir_all(&root).unwrap();
    let command = || {
        let mut cmd = Command::new(binary);
        cmd.env_clear().current_dir(&root);
        if let Some(system_root) = std::env::var_os("SystemRoot") {
            cmd.env("SystemRoot", system_root);
        }
        cmd
    };
    let file = if nob { ".env.nob" } else { ".env" };
    fs::write(root.join(file), "DISCORD_TOKEN=test-only-never-connect\nDISCORD_GUILD_ID=1\nDISCORD_CHANNEL_ID=2\nSTATE_DIR=state-must-not-exist\n").unwrap();
    if nob {
        fs::write(
            root.join(".env"),
            "invalid credential file must not be read\n",
        )
        .unwrap();
    }
    let output = command().arg("--check-config").output().unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("no connection or state writes"));
    assert!(!root.join("state-must-not-exist").exists());

    fs::write(
        root.join(file),
        "DISCORD_TOKEN=\"test-secret-marker-without-closing-quote\n",
    )
    .unwrap();
    let output = command().arg("--check-config").output().unwrap();
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("test-secret-marker"));
    assert!(command().arg("--help").output().unwrap().status.success());

    // An explicitly selected file is used instead of either default file.
    fs::write(root.join("custom.env"), "DISCORD_TOKEN=test-only-never-connect\nDISCORD_GUILD_ID=1\nDISCORD_CHANNEL_ID=2\nSTATE_DIR=custom-state-must-not-exist\n").unwrap();
    assert!(command()
        .args(["--env-file", "custom.env", "--check-config"])
        .output()
        .unwrap()
        .status
        .success());
    assert!(!root.join("custom-state-must-not-exist").exists());

    if nob {
        fs::remove_file(root.join(file)).unwrap();
        let output = command()
            .arg("--check-config")
            .env("NOB_DISCORD_GUILD_ID", "1")
            .env("NOB_DISCORD_CHANNEL_ID", "2")
            .env("DISCORD_TOKEN", "must-not-be-inherited")
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("DISCORD_TOKEN"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("must-not-be-inherited"));
    }
    fs::remove_dir_all(root).unwrap();
}
