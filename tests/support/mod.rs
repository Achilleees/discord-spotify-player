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

    // Exercise the volume parser through both real hosts without connecting or
    // creating state. Slightly over 100 must not round into the valid range.
    for (volume, valid) in [
        ("0", true),
        ("37.5", true),
        ("100", true),
        ("NaN", false),
        ("inf", false),
        ("-inf", false),
        ("-1", false),
        ("100.000001", false),
    ] {
        fs::write(
            root.join("volume.env"),
            format!("DISCORD_TOKEN=test-only-never-connect\nDISCORD_GUILD_ID=1\nDISCORD_CHANNEL_ID=2\nSTATE_DIR=volume-state-must-not-exist\nSOUNDBOARD_VOLUME_PERCENT={volume}\n"),
        )
        .unwrap();
        let output = command()
            .args(["--env-file", "volume.env", "--check-config"])
            .output()
            .unwrap();
        assert_eq!(output.status.success(), valid, "volume case {volume}");
        if valid {
            assert!(String::from_utf8_lossy(&output.stdout)
                .contains("no connection or state writes"));
        } else {
            assert!(String::from_utf8_lossy(&output.stderr)
                .contains("SOUNDBOARD_VOLUME_PERCENT"));
        }
        assert!(!root.join("volume-state-must-not-exist").exists());
    }

    // Both real entrypoints validate paired routing without binding sockets or
    // authenticating either Discord identity. All values here are synthetic.
    let base = "DISCORD_TOKEN=test-only-never-connect\nDISCORD_GUILD_ID=1\nDISCORD_CHANNEL_ID=2\nSTATE_DIR=paired-state-must-not-exist\n";
    let mode = if nob { "coordinator" } else { "worker" };
    let key = "01".repeat(32);
    let paired = format!("{base}COMMAND_MODE={mode}\nROUTING_LISTEN=127.0.0.1:19211\nROUTING_PEER=127.0.0.1:19212\nROUTING_KEY={key}\n");
    fs::write(root.join(file), &paired).unwrap();
    assert!(command()
        .arg("--check-config")
        .output()
        .unwrap()
        .status
        .success());
    assert!(!root.join("paired-state-must-not-exist").exists());
    for (from, to) in [
        ("127.0.0.1:19211", "0.0.0.0:19211"),
        ("127.0.0.1:19211", "127.0.0.1:0"),
        (key.as_str(), "invalid-routing-key-must-not-be-printed"),
        (mode, "unknown-role"),
        (mode, if nob { "worker" } else { "coordinator" }),
        ("ROUTING_KEY=", "UNUSED_KEY="),
    ] {
        fs::write(root.join(file), paired.replace(from, to)).unwrap();
        let result = command().arg("--check-config").output().unwrap();
        assert!(!result.status.success());
        assert!(!String::from_utf8_lossy(&result.stderr)
            .contains("invalid-routing-key-must-not-be-printed"));
    }
    if nob {
        fs::write(root.join(file), paired.replace("19212", "19211")).unwrap();
        assert!(!command()
            .arg("--check-config")
            .output()
            .unwrap()
            .status
            .success());
    }

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
