//! Process-level checks; every invocation here avoids hardware access.
use serde::Deserialize;
use std::process::Command;

#[derive(Deserialize)]
struct LedFixture {
    zone: String,
    mode: String,
    stdout: String,
    frame_hex: String,
}

#[test]
fn led_dry_runs_match_original_python() {
    // Captured from the original Python build_frame and dry-run output before porting.
    let fixtures: Vec<LedFixture> =
        serde_json::from_str(include_str!("fixtures/led-frames.json")).unwrap();
    assert_eq!(fixtures.len(), 77);
    for fixture in fixtures {
        let output = Command::new(env!("CARGO_BIN_EXE_evga"))
            .arg("led")
            .args(["mode", &fixture.mode, "-z", &fixture.zone, "--dry-run"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{} {}: {}",
            fixture.zone,
            fixture.mode,
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            fixture.stdout,
            "zone={} mode={}",
            fixture.zone,
            fixture.mode
        );
        let zone = match fixture.zone.as_str() {
            "all" => 31,
            "onboard" | "cover" => 16,
            "argb1" => 1,
            "argb2" => 2,
            "argb3" => 4,
            "argb4" => 8,
            _ => unreachable!(),
        };
        let mode = match fixture.mode.as_str() {
            "off" => 0,
            "on" => 1,
            "breathing" => 2,
            "rainbow" => 3,
            "pulse" => 4,
            "rainbowwave" | "rainbow-wave" => 5,
            "wave" => 6,
            "star" => 7,
            "colorstack" | "color-stack" => 8,
            _ => unreachable!(),
        };
        let frame = evga_z790_tools::led::build_frame(zone, mode, 0);
        let hex: String = frame.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, fixture.frame_hex, "full frame differs");
    }
}

#[test]
fn all_commands_offer_help_without_hardware() {
    for args in [
        vec!["--help"],
        vec!["led", "--help"],
        vec!["fan", "--help"],
        vec!["fan", "probe", "--help"],
        vec!["fan", "sweep", "--help"],
        vec!["fan", "mode", "--help"],
        vec!["fan", "manual", "--help"],
        vec!["fan", "auto", "--help"],
        vec!["fan", "sensor", "--help"],
        vec!["fan", "response", "--help"],
        vec!["fan", "save", "--help"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_evga"))
            .args(&args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("Usage:"));
    }
}

#[test]
fn color_and_save_dry_runs_work_without_a_device() {
    let color = Command::new(env!("CARGO_BIN_EXE_evga"))
        .args([
            "led",
            "--device",
            "/nonexistent-evga-test-hidraw",
            "color",
            "#123456",
            "--brightness",
            "128",
            "-z",
            "argb2",
            "--dry-run",
        ])
        .output()
        .unwrap();
    assert!(
        color.status.success(),
        "{}",
        String::from_utf8_lossy(&color.stderr)
    );
    let output = String::from_utf8(color.stdout).unwrap();
    // Independent packet check against StaticOnReport's five packed ARGB slots.
    assert!(
        output.starts_with("zone=0x02 mode=1 checksum=0x09\n"),
        "{output}"
    );
    assert!(
        output.contains("zone=0x02 color=#123456 brightness=128 checksum=0xEC\n"),
        "{output}"
    );
    assert!(output.contains("07 ea 02 03 00 00 00 ec 02 00 00 00 00 80 12 34 56 00 00 00 00 00 00 00 00 00 00 00 00 + 197 reserved bytes"), "{output}");

    let save = Command::new(env!("CARGO_BIN_EXE_evga"))
        .args([
            "led",
            "--device",
            "/nonexistent-evga-test-hidraw",
            "save",
            "--dry-run",
        ])
        .output()
        .unwrap();
    assert!(
        save.status.success(),
        "{}",
        String::from_utf8_lossy(&save.stderr)
    );
    assert_eq!(
        String::from_utf8(save.stdout).unwrap(),
        "04 ea 02 12 00 00 00 00 00 00 00 00 00 00 00 00 00\n"
    );
}

#[test]
fn new_fan_commands_reject_invalid_requests_before_opening_ports() {
    for (args, expected) in [
        (vec!["mode", "8", "pwm"], "out of range"),
        (vec!["manual", "0", "101"], "out of range"),
        (vec!["auto", "8"], "out of range"),
        (vec!["sensor", "8", "mb", "cpu"], "out of range"),
        (vec!["response", "0"], "at least one"),
        (vec!["response", "0", "--step-up", "8"], "out of range"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_evga"))
            .arg("fan")
            .args(&args)
            .output()
            .unwrap();
        assert!(!output.status.success(), "accepted {args:?}");
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains(expected), "{args:?}: {error}");
        assert!(
            !error.contains("/dev/port"),
            "opened hardware for {args:?}: {error}"
        );
    }
}

#[test]
fn probe_uses_its_device_without_requiring_a_state_directory() {
    let device = "/nonexistent-evga-test-port";
    let output = Command::new(env!("CARGO_BIN_EXE_evga"))
        .args(["fan", "probe", "--device", device])
        .env_remove("HOME")
        .env_remove("EVGA_FAN_STATE")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains(device),
        "probe did not use requested device: {error}"
    );
    assert!(
        !error.contains("HOME"),
        "probe required a state directory: {error}"
    );
}

#[test]
fn invalid_led_arguments_fail_without_hardware() {
    for args in [
        vec!["mode", "invalid", "--dry-run"],
        vec!["mode", "--dry-run"],
        vec!["on", "unexpected", "--dry-run"],
        vec!["off", "--zone", "invalid", "--dry-run"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_evga"))
            .arg("led")
            .args(&args)
            .output()
            .unwrap();
        assert!(!output.status.success(), "accepted {args:?}");
        assert!(!output.stderr.is_empty(), "no explanation for {args:?}");
    }
}

#[test]
fn fan_rejects_unsafe_inputs_before_opening_ports() {
    let cases = [
        ("8", "70", "100", "out of range"),
        ("0", "70", "101", "0..=100"),
        ("0", "80", "100", "100%"),
    ];
    for (fan, full_temp, full_pwm, message) in cases {
        let output = Command::new(env!("CARGO_BIN_EXE_evga"))
            .arg("fan")
            .args([
                "curve", fan, "0", "40", "50", "60", full_temp, "127", "127", "127", "45", "55",
                "65", "80", full_pwm, "100", "100", "100",
            ])
            .output()
            .unwrap();
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains(message), "expected {message:?}, got {error}");
        assert!(
            !error.contains("/dev/port"),
            "opened hardware before validation: {error}"
        );
    }
    let output = Command::new(env!("CARGO_BIN_EXE_evga"))
        .args(["fan", "sweep"])
        .arg("8")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("out of range"));
}

#[test]
fn malformed_restore_is_rejected_before_opening_ports() {
    let path = std::env::temp_dir().join(format!("evga-cli-restore-{}.json", std::process::id()));
    std::fs::write(&path, r#"{"acpi":[1],"smartfan":[2]}"#).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_evga"))
        .arg("fan")
        .arg("restore")
        .arg(&path)
        .output()
        .unwrap();
    std::fs::remove_file(path).unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("256"),
        "expected page-length validation, got {error}"
    );
    assert!(
        !error.contains("/dev/port"),
        "opened hardware before validation: {error}"
    );
}

#[test]
fn foreign_backup_is_rejected_before_opening_ports() {
    let path = std::env::temp_dir().join(format!("evga-cli-foreign-{}.json", std::process::id()));
    let body = serde_json::json!({"acpi": vec![0u8; 256], "smartfan": vec![0u8; 256]});
    std::fs::write(&path, serde_json::to_vec(&body).unwrap()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_evga"))
        .arg("fan")
        .arg("restore")
        .arg(&path)
        .output()
        .unwrap();
    std::fs::remove_file(path).unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("VendorID"),
        "expected backup identity validation, got {error}"
    );
    assert!(
        !error.contains("/dev/port"),
        "opened hardware before validation: {error}"
    );
}
