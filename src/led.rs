//! evga-led: control the onboard ARGB of an EVGA Z790 CLASSIFIED from Linux.
//!
//! Reverse engineered from EVGA ELEET X1 1.0.16.0:
//! `Library/EDispNetLib.dll` -> `EDispNetLib.Hardwares.Z790.{Led, LEDUsbCommand,
//! LEDModeReport, LEDMainCommand, LEDSubCommand, LEDZone}`.
//!
//! Transport: HID feature report ID 7 (225-byte payload / 226-byte buffer including
//! the report ID) on USB 3842:300e "EVGA 5516 MCU AP". Windows uses
//! `HidD_SetFeature` + `HidD_GetFeature`; the Linux equivalents are the
//! `HIDIOCSFEATURE` / `HIDIOCGFEATURE` ioctls on `/dev/hidrawN`.
//!
//! Frame (Pack=1):
//!
//! ```text
//! [0]      7                     report id
//! [1..2]   0xEA 0x02             header
//! [3]      1                     main command (LightEffect)
//! [4]      0 write / 1 read      sub command 1
//! [5]      0                     sub command 2 (None)
//! [6]      0xC0 success          response (filled by the device)
//! [7]      checksum              two's complement of sum(bytes 0..6, 8..13)
//! [8]      zone bitmask          1 ARGB1, 2 ARGB2, 4 ARGB3, 8 ARGB4,
//!                                16 ARGB_COVER (= onboard "Z790|CLASSIFIED"), 31 all
//! [9..13]  mode per zone         ARGB_1..ARGB_4, ARGB_COVER
//! [14..225] reserved
//! ```
//!
//! Hardware quirks verified on this board (BIOS 1.20, Nov 2026):
//! * A GET returns the MCU's last command frame, including right after a SET.
//!   The GET buffer's first byte must be the report ID (7): with 0 the
//!   interface, which declares only IDs 3-8, stalls the request with EPIPE.
//! * A SET on report ID 0 of the *other* interface (hidraw5) can wedge the LED
//!   interface until a USBDEVFS_RESET (0x5514) on the USB device.
//! * LED state survives a USB reset and reboots while standby power is present.
//!
//! Access: `/dev/hidrawN` is root-only. Install
//! `SUBSYSTEM=="hidraw", ATTRS{idVendor}=="3842", ATTRS{idProduct}=="300e", TAG+="uaccess"`
//! in /etc/udev/rules.d/ (60-evga-led.rules) so the logged-in user can open it.

use std::error::Error;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use clap::{Args, ValueEnum};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

pub const REPORT_ID: u8 = 0x07;
pub const HEADER1: u8 = 0xEA;
pub const HEADER2: u8 = 0x02;
pub const BUFLEN: usize = 226;
pub const MAIN_LIGHT_EFFECT: u8 = 1;
pub const SUB_WRITE: u8 = 0;
pub const SUB_READ: u8 = 1;
const VENDOR_PRODUCT: &str = "00003842:0000300E";
/// `85 07` in a HID report descriptor: Report ID 7.
const REPORT7_TAG: [u8; 2] = [0x85, 0x07];
const UDEV_RULE: &str =
    r#"SUBSYSTEM=="hidraw", ATTRS{idVendor}=="3842", ATTRS{idProduct}=="300e", TAG+="uaccess""#;

/// Zone names in frame order (bytes 9..13) with their bitmask bits.
pub const ZONE_ORDER: [(&str, u8); 5] = [
    ("argb1", 1),
    ("argb2", 2),
    ("argb3", 4),
    ("argb4", 8),
    ("cover", 16),
];
const ZONE_ALIASES: [(&str, u8); 7] = [
    ("all", 31),
    ("onboard", 16),
    ("cover", 16),
    ("argb1", 1),
    ("argb2", 2),
    ("argb3", 4),
    ("argb4", 8),
];
const MODES: [(&str, u8); 11] = [
    ("off", 0),
    ("on", 1),
    ("breathing", 2),
    ("rainbow", 3),
    ("pulse", 4),
    ("rainbowwave", 5),
    ("rainbow-wave", 5),
    ("wave", 6),
    ("star", 7),
    ("colorstack", 8),
    ("color-stack", 8),
];
const MODE_NAMES: [&str; 9] = [
    "off",
    "on",
    "breathing",
    "rainbow",
    "pulse",
    "rainbowwave",
    "wave",
    "star",
    "colorstack",
];

/// EVGA Z790 onboard ARGB control
#[derive(Args, Debug)]
pub struct Cli {
    /// hidraw node (default: autodetect)
    // A repeated --device takes the last value, as with argparse.
    #[arg(long, overrides_with = "device")]
    device: Option<PathBuf>,
    #[arg(value_enum, default_value_t = Action::Status)]
    action: Action,
    /// mode name for 'mode'
    mode: Option<String>,
    /// all | onboard | argb1..argb4
    #[arg(short, long, default_value = "all")]
    zone: String,
    /// print the frame without touching the device
    #[arg(short = 'n', long)]
    dry_run: bool,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Off,
    On,
    Mode,
    Status,
    Detect,
}

/// Run the parsed `led` command.
pub fn run(cli: Cli) -> Result<()> {
    if let Some(mode) = &cli.mode {
        if cli.action != Action::Mode {
            let action = cli.action.to_possible_value().expect("no skipped variants");
            return Err(
                format!("'{}' takes no mode name (got '{mode}')", action.get_name()).into(),
            );
        }
    }
    execute(&cli)
}

fn execute(cli: &Cli) -> Result<()> {
    match cli.action {
        Action::Detect => do_detect(&find_device(cli.device.as_deref())?),
        Action::Status => do_status(&find_device(cli.device.as_deref())?),
        Action::Off | Action::On | Action::Mode => {
            let zone = parse_zone(&cli.zone)?;
            let mode = match cli.action {
                Action::Off => 0,
                Action::On => 1,
                _ => parse_mode(cli.mode.as_deref().ok_or("'mode' needs a mode name")?)?,
            };
            // A dry run only prints the frame, so it must work without the board.
            let path = if cli.dry_run {
                None
            } else {
                Some(find_device(cli.device.as_deref())?)
            };
            do_set(path.as_deref(), zone, mode)
        }
    }
}

fn lookup(table: &[(&str, u8)], key: &str) -> Option<u8> {
    table.iter().find(|(name, _)| *name == key).map(|&(_, v)| v)
}

fn sorted_names(table: &[(&str, u8)]) -> String {
    let mut names: Vec<&str> = table.iter().map(|&(n, _)| n).collect();
    names.sort_unstable();
    names.dedup();
    names.join(", ")
}

fn parse_zone(name: &str) -> Result<u8> {
    lookup(&ZONE_ALIASES, &name.to_lowercase())
        .ok_or_else(|| format!("unknown zone '{name}': {}", sorted_names(&ZONE_ALIASES)).into())
}

fn parse_mode(name: &str) -> Result<u8> {
    lookup(&MODES, &name.to_lowercase())
        .ok_or_else(|| format!("unknown mode '{name}': {}", sorted_names(&MODES)).into())
}

fn mode_name(mode: u8) -> String {
    MODE_NAMES
        .get(usize::from(mode))
        .map_or_else(|| mode.to_string(), |n| n.to_string())
}

fn response_name(code: u8) -> String {
    match code {
        0xC0 => "Success".into(),
        0xC1 => "Fail".into(),
        0xC2 => "InProcess".into(),
        _ => format!("{code:#x}"),
    }
}

const fn ioc(direction: u32, typ: u8, nr: u32, size: usize) -> u32 {
    (direction << 30) | ((size as u32) << 16) | ((typ as u32) << 8) | nr
}

/// `HIDIOCSFEATURE(len)`: `_IOC(_IOC_WRITE|_IOC_READ, 'H', 0x06, len)`.
pub const fn hidiocsfeature(size: usize) -> u32 {
    ioc(3, b'H', 0x06, size)
}

/// `HIDIOCGFEATURE(len)`: `_IOC(_IOC_WRITE|_IOC_READ, 'H', 0x07, len)`.
pub const fn hidiocgfeature(size: usize) -> u32 {
    ioc(3, b'H', 0x07, size)
}

/// Build a report-7 LightEffect frame. Modes are written only for zones in `zone`.
pub fn build_frame(zone: u8, mode: u8, sub: u8) -> [u8; BUFLEN] {
    let mut buf = [0u8; BUFLEN];
    buf[..5].copy_from_slice(&[REPORT_ID, HEADER1, HEADER2, MAIN_LIGHT_EFFECT, sub]);
    buf[8] = zone;
    if sub == SUB_WRITE {
        for (idx, &(_, bit)) in ZONE_ORDER.iter().enumerate() {
            if zone & bit != 0 {
                buf[9 + idx] = mode;
            }
        }
    }
    let sum = buf[0..7]
        .iter()
        .chain(&buf[8..14])
        .fold(0u8, |acc, &b| acc.wrapping_add(b));
    buf[7] = sum.wrapping_neg();
    buf
}

/// Pick the hidraw node of 3842:300e, preferring the interface that declares report 7.
fn find_device(explicit: Option<&Path>) -> Result<PathBuf> {
    match explicit {
        Some(path) => Ok(path.to_path_buf()),
        None => find_device_in(Path::new("/dev"), Path::new("/sys/class/hidraw")),
    }
}

fn find_device_in(dev: &Path, sys_hidraw: &Path) -> Result<PathBuf> {
    let mut nodes: Vec<PathBuf> = fs::read_dir(dev)?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("hidraw"))
        .map(|e| e.path())
        .collect();
    nodes.sort();
    let mut fallback = None;
    for node in nodes {
        let name = node.file_name().expect("read_dir entries have names");
        let device = sys_hidraw.join(name).join("device");
        let Ok(uevent) = fs::read_to_string(device.join("uevent")) else {
            continue;
        };
        if !uevent.to_uppercase().contains(VENDOR_PRODUCT) {
            continue;
        }
        let desc = fs::read(device.join("report_descriptor")).unwrap_or_default();
        if has_report7(&desc) {
            return Ok(node);
        }
        fallback.get_or_insert(node);
    }
    fallback.ok_or_else(|| "no EVGA 3842:300e hidraw device found".into())
}

fn has_report7(desc: &[u8]) -> bool {
    desc.windows(2).any(|w| w == REPORT7_TAG)
}

fn descriptor_path(node: &Path) -> PathBuf {
    let name = node.file_name().unwrap_or(node.as_os_str());
    Path::new("/sys/class/hidraw")
        .join(name)
        .join("device/report_descriptor")
}

/// Open the node read-write; permission denied explains the udev rule.
fn open_node(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| {
            if e.kind() == io::ErrorKind::PermissionDenied {
                format!(
                    "{}: permission denied on the hidraw node; install the udev rule \
                     60-evga-led.rules ({UDEV_RULE}) in /etc/udev/rules.d/, then reload udev \
                     and replug or trigger",
                    path.display()
                )
            } else {
                format!("{}: {e}", path.display())
            }
            .into()
        })
}

fn feature_ioctl(file: &File, request: u32, buf: &mut [u8; BUFLEN]) -> io::Result<()> {
    // SAFETY: `buf` is a live, writable BUFLEN-byte buffer, matching the size
    // encoded in `request`.
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), request as _, buf.as_mut_ptr()) };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Symbolic errno name like Python's `errno.errorcode`, else the number.
fn errno_name(err: &io::Error) -> String {
    let Some(code) = err.raw_os_error() else {
        return err.to_string();
    };
    let name = match code {
        libc::EPERM => "EPERM",
        libc::ENOENT => "ENOENT",
        libc::EINTR => "EINTR",
        libc::EIO => "EIO",
        libc::ENXIO => "ENXIO",
        libc::EBADF => "EBADF",
        libc::EAGAIN => "EAGAIN",
        libc::ENOMEM => "ENOMEM",
        libc::EACCES => "EACCES",
        libc::EFAULT => "EFAULT",
        libc::EBUSY => "EBUSY",
        libc::ENODEV => "ENODEV",
        libc::EINVAL => "EINVAL",
        libc::ENOTTY => "ENOTTY",
        libc::EPIPE => "EPIPE",
        libc::EPROTO => "EPROTO",
        libc::EOPNOTSUPP => "EOPNOTSUPP",
        libc::ESHUTDOWN => "ESHUTDOWN",
        libc::ETIMEDOUT => "ETIMEDOUT",
        _ => return code.to_string(),
    };
    name.into()
}

/// GET report 7: the MCU echoes its last command frame.
fn read_state(path: &Path) -> Option<[u8; BUFLEN]> {
    let file = match open_node(path) {
        Ok(file) => file,
        Err(e) => {
            println!("read-back unavailable ({e})");
            return None;
        }
    };
    let mut buf = [0u8; BUFLEN];
    buf[0] = REPORT_ID; // hidraw reads the requested report ID from here
    match feature_ioctl(&file, hidiocgfeature(BUFLEN), &mut buf) {
        Ok(()) => Some(buf),
        Err(e) => {
            println!("read-back failed: {}", errno_name(&e));
            None
        }
    }
}

fn hex_bytes(bytes: &[u8]) -> String {
    let parts: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
    parts.join(" ")
}

fn dry_run_lines(zone: u8, mode: u8, frame: &[u8; BUFLEN]) -> [String; 2] {
    [
        format!("zone=0x{zone:02X} mode={mode} checksum=0x{:02X}", frame[7]),
        format!(
            "{} + {} reserved bytes",
            hex_bytes(&frame[..20]),
            BUFLEN - 20
        ),
    ]
}

fn read_back_line(state: &[u8; BUFLEN]) -> String {
    let modes: Vec<String> = state[9..14].iter().map(u8::to_string).collect();
    format!(
        "read back: zone=0x{:02X} modes=[{}] response={}",
        state[8],
        modes.join(", "),
        response_name(state[6])
    )
}

fn status_lines(state: &[u8; BUFLEN]) -> Vec<String> {
    let zones: Vec<&str> = ZONE_ORDER
        .iter()
        .filter(|&&(_, bit)| state[8] & bit != 0)
        .map(|&(name, _)| name)
        .collect();
    let zones = if zones.is_empty() {
        "none".to_string()
    } else {
        zones.join(", ")
    };
    let mut lines = vec![format!(
        "last command: zone=0x{:02X} ({zones}) response={}",
        state[8],
        response_name(state[6])
    )];
    for (idx, &(name, _)) in ZONE_ORDER.iter().enumerate() {
        lines.push(format!("  {name:6} {}", mode_name(state[9 + idx])));
    }
    lines
}

fn do_set(path: Option<&Path>, zone: u8, mode: u8) -> Result<()> {
    let mut frame = build_frame(zone, mode, SUB_WRITE);
    let Some(path) = path else {
        for line in dry_run_lines(zone, mode, &frame) {
            println!("{line}");
        }
        return Ok(());
    };

    let file = open_node(path)?;
    let checksum = frame[7];
    feature_ioctl(&file, hidiocsfeature(BUFLEN), &mut frame)
        .map_err(|e| format!("SET failed: {}", errno_name(&e)))?;
    drop(file);

    println!(
        "sent: zone 0x{zone:02X} -> {} (checksum 0x{checksum:02X})",
        mode_name(mode)
    );
    if let Some(state) = read_state(path) {
        println!("{}", read_back_line(&state));
    }
    Ok(())
}

fn do_status(path: &Path) -> Result<()> {
    drop(open_node(path)?);
    let state = read_state(path).ok_or("status read failed")?;
    for line in status_lines(&state) {
        println!("{line}");
    }
    Ok(())
}

fn do_detect(path: &Path) -> Result<()> {
    println!("device: {}", path.display());
    match fs::read(descriptor_path(path)) {
        Ok(desc) => println!(
            "report id 7 present: {}",
            if has_report7(&desc) { "True" } else { "False" }
        ),
        Err(e) => println!("descriptor unreadable here: {e}"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// `Cli` is flattened into the `evga led` subcommand; parse it the same way.
    #[derive(clap::Parser)]
    struct Root {
        #[command(flatten)]
        cli: Cli,
    }

    fn parse(args: &[&str]) -> std::result::Result<Cli, clap::Error> {
        use clap::Parser;
        Root::try_parse_from(std::iter::once("led").chain(args.iter().copied())).map(|r| r.cli)
    }

    #[test]
    fn all_off_frame_matches_documented_bytes() {
        // README: "Everything off (checksum 0xED): 07 ea 02 01 00 00 00 ed 1f ..."
        let f = build_frame(31, 0, SUB_WRITE);
        assert_eq!(f.len(), 226);
        assert_eq!(
            &f[..9],
            &[0x07, 0xEA, 0x02, 0x01, 0x00, 0x00, 0x00, 0xED, 0x1F]
        );
        assert!(f[9..].iter().all(|&b| b == 0));
    }

    #[test]
    fn mode_lands_only_in_selected_zone_and_checksum_covers_it() {
        // onboard on: 7+0xEA+2+1 = 0xF4, +0x10 zone +1 mode = 261 -> -261 mod 256 = 0xFB.
        let f = build_frame(16, 1, SUB_WRITE);
        assert_eq!(&f[9..14], &[0, 0, 0, 0, 1]);
        assert_eq!(f[7], 0xFB);
        let f = build_frame(4, 3, SUB_WRITE);
        assert_eq!(&f[9..14], &[0, 0, 3, 0, 0]);
        let sum: u32 = f[..14].iter().map(|&b| u32::from(b)).sum();
        assert_eq!(
            sum % 256,
            0,
            "bytes 0..13 including checksum sum to 0 mod 256"
        );
    }

    #[test]
    fn read_frame_carries_no_modes() {
        let f = build_frame(31, 5, SUB_READ);
        assert_eq!(f[4], SUB_READ);
        assert_eq!(&f[9..14], &[0; 5]);
    }

    #[test]
    fn ioctl_numbers_match_linux_hidraw() {
        // From <linux/hidraw.h> with _IOC_WRITE|_IOC_READ = 3 and len 226 = 0xE2.
        assert_eq!(hidiocsfeature(BUFLEN), 0xC0E2_4806);
        assert_eq!(hidiocgfeature(BUFLEN), 0xC0E2_4807);
    }

    #[test]
    fn zone_and_mode_aliases_are_case_insensitive() {
        assert_eq!(parse_zone("ALL").unwrap(), 31);
        assert_eq!(parse_zone("Onboard").unwrap(), 16);
        assert_eq!(parse_zone("cover").unwrap(), 16);
        assert_eq!(parse_zone("argb4").unwrap(), 8);
        assert_eq!(parse_mode("Rainbow-Wave").unwrap(), 5);
        assert_eq!(parse_mode("color-stack").unwrap(), 8);
        assert_eq!(parse_mode("colorstack").unwrap(), 8);
    }

    #[test]
    fn unknown_names_list_sorted_choices() {
        assert_eq!(
            parse_zone("argb5").unwrap_err().to_string(),
            "unknown zone 'argb5': all, argb1, argb2, argb3, argb4, cover, onboard"
        );
        assert_eq!(
            parse_mode("strobe").unwrap_err().to_string(),
            "unknown mode 'strobe': breathing, color-stack, colorstack, off, on, pulse, \
             rainbow, rainbow-wave, rainbowwave, star, wave"
        );
    }

    #[test]
    fn cli_defaults_and_options() {
        let cli = parse(&[]).unwrap();
        assert_eq!(cli.action, Action::Status);
        assert_eq!(cli.zone, "all");
        assert!(!cli.dry_run && cli.device.is_none() && cli.mode.is_none());

        let cli = parse(&["-z", "onboard", "mode", "rainbow", "-n"]).unwrap();
        assert_eq!(cli.action, Action::Mode);
        assert_eq!(cli.mode.as_deref(), Some("rainbow"));
        assert_eq!(cli.zone, "onboard");
        assert!(cli.dry_run);

        assert!(parse(&["blink"]).is_err());
    }

    #[test]
    fn repeated_device_takes_last_value() {
        // argparse semantics: the last --device wins rather than an error.
        let cli = parse(&[
            "--device",
            "/dev/hidraw9",
            "off",
            "--device",
            "/dev/hidraw4",
        ])
        .unwrap();
        assert_eq!(cli.device.as_deref(), Some(Path::new("/dev/hidraw4")));
    }

    #[test]
    fn dry_run_needs_no_device_and_bad_requests_return_errors() {
        let run_args = |args: &[&str]| run(parse(args).unwrap()).map_err(|e| e.to_string());
        assert!(run_args(&["--device", "/nonexistent/hidraw0", "-n", "off"]).is_ok());
        assert_eq!(
            run_args(&["-n", "mode"]).unwrap_err(),
            "'mode' needs a mode name"
        );
        assert!(run_args(&["-n", "-z", "bogus", "on"])
            .unwrap_err()
            .starts_with("unknown zone 'bogus'"));
        // Checked before anything else, even before the device is looked up.
        assert_eq!(
            run_args(&["--device", "/nonexistent/hidraw0", "on", "extra"]).unwrap_err(),
            "'on' takes no mode name (got 'extra')"
        );
        assert_eq!(
            run_args(&["status", "x"]).unwrap_err(),
            "'status' takes no mode name (got 'x')"
        );
    }

    #[test]
    fn permission_denied_explains_the_udev_rule() {
        use std::os::unix::fs::PermissionsExt;
        // root opens any file, so the check below would be meaningless.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let t = TempTree::new();
        let node = t.0.join("dev/hidraw0");
        fs::write(&node, b"").unwrap();
        fs::set_permissions(&node, fs::Permissions::from_mode(0o000)).unwrap();
        let path = node.to_str().unwrap();
        let err = run(parse(&["--device", path, "off"]).unwrap())
            .unwrap_err()
            .to_string();
        assert!(err.contains("permission denied"), "got {err}");
        assert!(
            err.contains("60-evga-led.rules") && err.contains(UDEV_RULE),
            "got {err}"
        );
        let err = run(parse(&["--device", path, "status"]).unwrap())
            .unwrap_err()
            .to_string();
        assert!(err.contains(UDEV_RULE), "got {err}");
    }

    #[test]
    fn output_lines_match_python_format() {
        let f = build_frame(31, 0, SUB_WRITE);
        assert_eq!(
            dry_run_lines(31, 0, &f),
            [
                "zone=0x1F mode=0 checksum=0xED".to_string(),
                "07 ea 02 01 00 00 00 ed 1f 00 00 00 00 00 00 00 00 00 00 00 + 206 reserved bytes"
                    .to_string(),
            ]
        );

        let mut state = build_frame(0x11, 0, SUB_WRITE);
        state[6] = 0xC0;
        state[9..14].copy_from_slice(&[2, 0, 0, 0, 12]);
        assert_eq!(
            read_back_line(&state),
            "read back: zone=0x11 modes=[2, 0, 0, 0, 12] response=Success"
        );
        assert_eq!(
            status_lines(&state),
            [
                "last command: zone=0x11 (argb1, cover) response=Success",
                "  argb1  breathing",
                "  argb2  off",
                "  argb3  off",
                "  argb4  off",
                "  cover  12",
            ]
        );
        state[8] = 0;
        state[6] = 0;
        assert_eq!(
            status_lines(&state)[0],
            "last command: zone=0x00 (none) response=0x0"
        );
    }

    #[test]
    fn errno_names() {
        assert_eq!(
            errno_name(&io::Error::from_raw_os_error(libc::EPIPE)),
            "EPIPE"
        );
        assert_eq!(errno_name(&io::Error::from_raw_os_error(4095)), "4095");
    }

    struct TempTree(PathBuf);

    impl TempTree {
        fn new() -> Self {
            static N: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "evga-led-test-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(dir.join("dev")).unwrap();
            Self(dir)
        }

        fn node(&self, name: &str, uevent: Option<&str>, desc: &[u8]) {
            fs::write(self.0.join("dev").join(name), b"").unwrap();
            let device = self.0.join("sys").join(name).join("device");
            fs::create_dir_all(&device).unwrap();
            if let Some(uevent) = uevent {
                fs::write(device.join("uevent"), uevent).unwrap();
            }
            fs::write(device.join("report_descriptor"), desc).unwrap();
        }

        fn find(&self) -> Result<PathBuf> {
            find_device_in(&self.0.join("dev"), &self.0.join("sys"))
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    const EVGA: &str = "DRIVER=hid-generic\nHID_ID=0003:00003842:0000300e\n";

    #[test]
    fn discovery_prefers_interface_with_report7() {
        let t = TempTree::new();
        t.node(
            "hidraw0",
            Some("HID_ID=0003:0000046D:0000C52B\n"),
            &[0x85, 0x07],
        );
        t.node("hidraw10", Some(EVGA), &[0x06, 0x00, 0xFF, 0x85, 0x03]);
        t.node("hidraw2", Some(EVGA), &[0x06, 0x00, 0xFF, 0x85, 0x07]);
        t.node("hidraw3", None, &[0x85, 0x07]);
        assert_eq!(t.find().unwrap(), t.0.join("dev/hidraw2"));
    }

    #[test]
    fn discovery_falls_back_to_first_matching_node_in_name_order() {
        let t = TempTree::new();
        // Lexical order, like Python's sorted(Path.glob): hidraw10 < hidraw2.
        t.node("hidraw2", Some(EVGA), &[0x85, 0x03]);
        t.node("hidraw10", Some(EVGA), &[0x85, 0x04]);
        assert_eq!(t.find().unwrap(), t.0.join("dev/hidraw10"));
    }

    #[test]
    fn discovery_reports_missing_board() {
        let t = TempTree::new();
        t.node(
            "hidraw0",
            Some("HID_ID=0003:0000046D:0000C52B\n"),
            &[0x85, 0x07],
        );
        assert_eq!(
            t.find().unwrap_err().to_string(),
            "no EVGA 3842:300e hidraw device found"
        );
    }
}
