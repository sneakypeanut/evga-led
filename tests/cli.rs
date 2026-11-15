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
        let output = Command::new(env!("CARGO_BIN_EXE_evga-led"))
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
fn all_binaries_offer_help_without_hardware() {
    for binary in [
        env!("CARGO_BIN_EXE_evga-led"),
        env!("CARGO_BIN_EXE_evga-fan"),
        env!("CARGO_BIN_EXE_evga-fan-probe"),
        env!("CARGO_BIN_EXE_evga-fan-sweep"),
    ] {
        let output = Command::new(binary).arg("--help").output().unwrap();
        assert!(
            output.status.success(),
            "{binary}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("Usage:"));
    }
}

#[test]
fn invalid_led_arguments_fail_without_hardware() {
    for args in [
        vec!["mode", "invalid", "--dry-run"],
        vec!["mode", "--dry-run"],
        vec!["on", "unexpected", "--dry-run"],
        vec!["off", "--zone", "invalid", "--dry-run"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_evga-led"))
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
        let output = Command::new(env!("CARGO_BIN_EXE_evga-fan"))
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
    let output = Command::new(env!("CARGO_BIN_EXE_evga-fan-sweep"))
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
    let output = Command::new(env!("CARGO_BIN_EXE_evga-fan"))
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
    let output = Command::new(env!("CARGO_BIN_EXE_evga-fan"))
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
