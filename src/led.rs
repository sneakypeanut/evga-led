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
//! * Report 4 `LEDDarkMode` (0x50) is a global switch over every zone: with
//!   data 0 (dark) report 7 still answers Success but nothing lights. ELeet X1's
//!   "RGB On" toggle writes data 1; reading it is a SET with subcommand 1 then
//!   a GET (`Data1Report`: 7-byte command, data byte, 9 reserved, no checksum).
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
/// Report 4 carries the general commands, including the global LED switch.
pub const GENERAL_REPORT_ID: u8 = 0x04;
pub const GENERAL_LEN: usize = 17;
/// `GeneralMainCommand.LEDDarkMode`: data 0 = dark (every zone off), 1 = lit.
pub const MAIN_LED_DARK_MODE: u8 = 0x50;
const RESP_SUCCESS: u8 = 0xC0;
const RESP_IN_PROCESS: u8 = 0xC2;
/// ELeet X1 waits 5 ms between the SET and the GET of every command.
const SET_GET_DELAY: std::time::Duration = std::time::Duration::from_millis(5);
const IN_PROCESS_RETRIES: usize = 10;
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

/// HID feature-report transport: the hidraw node, or a model in tests.
trait Feature {
    fn set_feature(&mut self, buf: &mut [u8]) -> io::Result<()>;
    /// `buf[0]` names the report ID to fetch.
    fn get_feature(&mut self, buf: &mut [u8]) -> io::Result<()>;
}

struct Hidraw(File);

impl Hidraw {
    /// Returns the byte count the hidraw ioctl reports.
    fn ioctl(&self, request: u32, buf: &mut [u8]) -> io::Result<usize> {
        // SAFETY: `buf` is a live, writable buffer of exactly the length
        // encoded in `request`.
        let rc = unsafe { libc::ioctl(self.0.as_raw_fd(), request as _, buf.as_mut_ptr()) };
        usize::try_from(rc).map_err(|_| io::Error::last_os_error())
    }
}

/// A transfer is only usable in full: a short GET would leave zeros that
/// read as data (dark switch, mode off).
fn require_full(count: usize, len: usize) -> io::Result<()> {
    if count == len {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!("short transfer: {count} of {len} bytes"),
        ))
    }
}

impl Feature for Hidraw {
    fn set_feature(&mut self, buf: &mut [u8]) -> io::Result<()> {
        let count = self.ioctl(hidiocsfeature(buf.len()), buf)?;
        require_full(count, buf.len())
    }

    fn get_feature(&mut self, buf: &mut [u8]) -> io::Result<()> {
        let count = self.ioctl(hidiocgfeature(buf.len()), buf)?;
        require_full(count, buf.len())
    }
}

/// Build a report-4 `Data1Report`: the 7-byte general command, one data byte
/// and 9 reserved bytes. Unlike report 7 it carries no checksum.
pub fn build_general_frame(main: u8, sub: u8, data: u8) -> [u8; GENERAL_LEN] {
    let mut buf = [0u8; GENERAL_LEN];
    buf[..5].copy_from_slice(&[GENERAL_REPORT_ID, HEADER1, HEADER2, main, sub]);
    buf[7] = data;
    buf
}

/// The vendor's `SetAndGetFeature(ref s, 5)`: SET `frame`, wait 5 ms, GET the
/// same report ID, and check that the reply echoes the command header. A
/// `Success` reply replaces `frame`; `InProcess` is re-read a bounded number of
/// times; anything else is an error.
fn transact<D: Feature>(dev: &mut D, frame: &mut [u8]) -> Result<()> {
    let id = frame[0];
    dev.set_feature(frame)
        .map_err(|e| format!("SET failed: {}", errno_name(&e)))?;
    let mut reply = vec![0u8; frame.len()];
    for _ in 0..=IN_PROCESS_RETRIES {
        std::thread::sleep(SET_GET_DELAY);
        reply.fill(0);
        reply[0] = id; // hidraw reads the requested report ID from here
        dev.get_feature(&mut reply)
            .map_err(|e| format!("report {id} read-back failed: {}", errno_name(&e)))?;
        if reply[..6] != frame[..6] {
            return Err(format!(
                "report {id}: reply {} does not echo command {}",
                hex_bytes(&reply[..7]),
                hex_bytes(&frame[..6])
            )
            .into());
        }
        match reply[6] {
            RESP_SUCCESS => {
                frame.copy_from_slice(&reply);
                return Ok(());
            }
            RESP_IN_PROCESS => continue,
            code => {
                return Err(format!(
                    "report {id} command 0x{:02X}: device answered {}",
                    frame[3],
                    response_name(code)
                )
                .into())
            }
        }
    }
    Err(format!(
        "report {id} command 0x{:02X}: still InProcess after {} reads",
        frame[3],
        IN_PROCESS_RETRIES + 1
    )
    .into())
}

/// Read the global LED switch (report 4 `LEDDarkMode`); `true` means dark.
fn read_dark<D: Feature>(dev: &mut D) -> Result<bool> {
    let mut frame = build_general_frame(MAIN_LED_DARK_MODE, SUB_READ, 0);
    transact(dev, &mut frame)?;
    match frame[7] {
        0 => Ok(true),
        1 => Ok(false),
        other => Err(format!(
            "global LED switch (report 4 LEDDarkMode) reads {other}, expected 0 or 1"
        )
        .into()),
    }
}

/// Turn the global LED switch on, as ELeet X1's "RGB On" toggle does, and
/// confirm it took.
fn clear_dark<D: Feature>(dev: &mut D) -> Result<()> {
    let mut frame = build_general_frame(MAIN_LED_DARK_MODE, SUB_WRITE, 1);
    transact(dev, &mut frame)?;
    if read_dark(dev)? {
        return Err(
            "global LED switch (report 4 LEDDarkMode) still reads dark after turning it on".into(),
        );
    }
    Ok(())
}

/// Read every zone's actual mode with a report-7 LightEffect Read query.
fn read_zone_modes<D: Feature>(dev: &mut D) -> Result<[u8; 5]> {
    let mut frame = build_frame(0, 0, SUB_READ);
    transact(dev, &mut frame)?;
    let mut modes = [0u8; 5];
    modes.copy_from_slice(&frame[9..14]);
    Ok(modes)
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

fn status_lines(dark: bool, modes: &[u8; 5]) -> Vec<String> {
    let mut lines = vec![if dark {
        "global LED switch: dark (all zones forced off; run `evga led on` to enable lights)"
            .to_string()
    } else {
        "global LED switch: lit".to_string()
    }];
    lines.push("zone modes (read from the MCU):".to_string());
    for (&(name, _), &mode) in ZONE_ORDER.iter().zip(modes) {
        lines.push(format!("  {name:6} {}", mode_name(mode)));
    }
    lines
}

fn do_set(path: Option<&Path>, zone: u8, mode: u8) -> Result<()> {
    let Some(path) = path else {
        let frame = build_frame(zone, mode, SUB_WRITE);
        for line in dry_run_lines(zone, mode, &frame) {
            println!("{line}");
        }
        return Ok(());
    };
    set_zones(&mut Hidraw(open_node(path)?), zone, mode)
}

/// Write `mode` to the zones in `zone`. A nonzero mode also turns the global
/// LED switch on when it is dark, since a dark switch overrides every zone;
/// mode 0 never touches the switch, so other zones keep their state.
fn set_zones<D: Feature>(dev: &mut D, zone: u8, mode: u8) -> Result<()> {
    let mut frame = build_frame(zone, mode, SUB_WRITE);
    let checksum = frame[7];
    transact(dev, &mut frame)?;
    println!(
        "sent: zone 0x{zone:02X} -> {} (checksum 0x{checksum:02X})",
        mode_name(mode)
    );
    println!("{}", read_back_line(&frame));
    if mode != 0 && read_dark(dev)? {
        clear_dark(dev)?;
        println!("global LED switch: was dark, now lit");
    }
    Ok(())
}

fn do_status(path: &Path) -> Result<()> {
    for line in status_report(&mut Hidraw(open_node(path)?))? {
        println!("{line}");
    }
    Ok(())
}

fn status_report<D: Feature>(dev: &mut D) -> Result<Vec<String>> {
    let dark = read_dark(dev)?;
    let modes = read_zone_modes(dev)?;
    Ok(status_lines(dark, &modes))
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
            status_lines(true, &[2, 0, 0, 0, 12]),
            [
                "global LED switch: dark (all zones forced off; run `evga led on` to enable lights)",
                "zone modes (read from the MCU):",
                "  argb1  breathing",
                "  argb2  off",
                "  argb3  off",
                "  argb4  off",
                "  cover  12",
            ]
        );
        assert_eq!(status_lines(false, &[0; 5])[0], "global LED switch: lit");
    }

    #[test]
    fn short_transfers_are_errors() {
        assert!(require_full(GENERAL_LEN, GENERAL_LEN).is_ok());
        assert!(require_full(BUFLEN, BUFLEN).is_ok());
        let err = require_full(7, GENERAL_LEN).unwrap_err();
        assert_eq!(err.to_string(), "short transfer: 7 of 17 bytes");
        assert!(require_full(0, BUFLEN).is_err());
    }

    #[test]
    fn report4_frames_match_vendor_data1report() {
        // Data1Report: 04 EA 02 <main> <sub1> <sub2> <resp> <data> + 9 reserved,
        // no checksum. Bytes Hamdi sent live to light the board, and the query.
        assert_eq!(
            build_general_frame(MAIN_LED_DARK_MODE, SUB_WRITE, 1),
            [4, 0xEA, 2, 0x50, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(
            build_general_frame(MAIN_LED_DARK_MODE, SUB_READ, 0),
            [4, 0xEA, 2, 0x50, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
        );
        // _IOC(3, 'H', 6/7, 17)
        assert_eq!(hidiocsfeature(GENERAL_LEN), 0xC011_4806);
        assert_eq!(hidiocgfeature(GENERAL_LEN), 0xC011_4807);
        // The report-7 zone query captured live: 07 ea 02 01 01 00 00 0b 00 ...
        let q = build_frame(0, 0, SUB_READ);
        assert_eq!(&q[..9], &[7, 0xEA, 2, 1, 1, 0, 0, 0x0B, 0]);
    }

    /// Model of the LED MCU's two feature reports, answering the way the board
    /// did live: a GET returns the reply to the last SET of that report ID.
    #[derive(Default)]
    struct Mcu {
        dark: bool,
        modes: [u8; 5],
        replies: std::collections::HashMap<u8, Vec<u8>>,
        sets: Vec<Vec<u8>>,
        /// Answer InProcess to this many GETs before the real reply.
        in_process: usize,
        /// Force this response byte on every reply to a SET of report 7.
        report7_response: Option<u8>,
        /// Echo a different main command on report 7 replies.
        garble_report7: bool,
        /// Ignore writes to the global switch.
        stuck_dark: bool,
        /// Report this data byte for the switch instead of 0/1.
        dark_data: Option<u8>,
        /// Echo a nonzero subcommand 2 on every reply.
        garble_sub2: bool,
    }

    impl Mcu {
        fn dark_writes(&self) -> Vec<u8> {
            self.sets
                .iter()
                .filter(|f| {
                    f[0] == GENERAL_REPORT_ID && f[3] == MAIN_LED_DARK_MODE && f[4] == SUB_WRITE
                })
                .map(|f| f[7])
                .collect()
        }
    }

    impl Feature for Mcu {
        fn set_feature(&mut self, buf: &mut [u8]) -> io::Result<()> {
            self.sets.push(buf.to_vec());
            let mut reply = buf.to_vec();
            reply[6] = RESP_SUCCESS;
            match (buf[0], buf[3], buf[4]) {
                (REPORT_ID, MAIN_LIGHT_EFFECT, SUB_WRITE) => {
                    for (idx, &(_, bit)) in ZONE_ORDER.iter().enumerate() {
                        if buf[8] & bit != 0 {
                            self.modes[idx] = buf[9 + idx];
                        }
                    }
                    if let Some(code) = self.report7_response {
                        reply[6] = code;
                    }
                    if self.garble_report7 {
                        reply[3] = 0x05;
                    }
                }
                (REPORT_ID, MAIN_LIGHT_EFFECT, SUB_READ) => {
                    reply[9..14].copy_from_slice(&self.modes);
                    reply[7] = 0x46; // the device fills its own checksum
                }
                (GENERAL_REPORT_ID, MAIN_LED_DARK_MODE, SUB_WRITE) => {
                    if !self.stuck_dark {
                        self.dark = buf[7] == 0;
                    }
                }
                (GENERAL_REPORT_ID, MAIN_LED_DARK_MODE, SUB_READ) => {
                    reply[7] = self.dark_data.unwrap_or(u8::from(!self.dark));
                }
                _ => reply[6] = 0xC1,
            }
            if self.garble_sub2 {
                reply[5] = 1;
            }
            self.replies.insert(buf[0], reply);
            Ok(())
        }

        fn get_feature(&mut self, buf: &mut [u8]) -> io::Result<()> {
            let reply = self
                .replies
                .get(&buf[0])
                .ok_or_else(|| io::Error::from_raw_os_error(libc::EPIPE))?;
            buf.copy_from_slice(reply);
            if self.in_process > 0 {
                self.in_process -= 1;
                buf[6] = RESP_IN_PROCESS;
            }
            Ok(())
        }
    }

    #[test]
    fn nonzero_mode_turns_a_dark_global_switch_on() {
        let mut mcu = Mcu {
            dark: true,
            ..Mcu::default()
        };
        set_zones(&mut mcu, 16, 1).unwrap();
        assert_eq!(mcu.modes, [0, 0, 0, 0, 1]);
        assert!(!mcu.dark, "switch must be lit after `on`");
        assert_eq!(mcu.dark_writes(), [1]);
        // The zone write happens before the switch is touched.
        assert_eq!(mcu.sets[0][0], REPORT_ID);
    }

    #[test]
    fn lit_switch_is_left_alone_and_off_never_touches_it() {
        let mut mcu = Mcu::default();
        set_zones(&mut mcu, 31, 3).unwrap();
        assert!(mcu.dark_writes().is_empty(), "already lit: no switch write");

        for dark in [false, true] {
            let mut mcu = Mcu {
                dark,
                modes: [1; 5],
                ..Mcu::default()
            };
            set_zones(&mut mcu, 16, 0).unwrap();
            assert_eq!(mcu.modes, [1, 1, 1, 1, 0], "other zones keep their mode");
            assert_eq!(mcu.dark, dark);
            assert!(
                mcu.sets.iter().all(|f| f[0] == REPORT_ID),
                "mode 0 must not read or write report 4"
            );
        }
    }

    #[test]
    fn status_reads_the_switch_and_real_zone_modes() {
        let mut mcu = Mcu {
            dark: true,
            modes: [2, 0, 0, 0, 1],
            ..Mcu::default()
        };
        let lines = status_report(&mut mcu).unwrap();
        assert!(lines[0].contains("dark"), "{lines:?}");
        assert_eq!(lines[2], "  argb1  breathing");
        assert_eq!(lines[6], "  cover  on");
        // Queries only: nothing was written to a zone or the switch.
        assert!(mcu.sets.iter().all(|f| f[4] == SUB_READ));
        assert!(mcu.dark);
    }

    #[test]
    fn in_process_is_retried_within_a_bound() {
        let mut mcu = Mcu {
            dark: true,
            in_process: 3,
            ..Mcu::default()
        };
        set_zones(&mut mcu, 31, 1).unwrap();
        assert!(!mcu.dark);

        let mut mcu = Mcu {
            in_process: usize::MAX,
            ..Mcu::default()
        };
        let err = set_zones(&mut mcu, 31, 1).unwrap_err().to_string();
        assert!(err.contains("still InProcess"), "{err}");
    }

    #[test]
    fn failed_or_malformed_replies_are_errors_not_success() {
        let mut mcu = Mcu {
            dark: true,
            report7_response: Some(0xC1),
            ..Mcu::default()
        };
        let err = set_zones(&mut mcu, 31, 1).unwrap_err().to_string();
        assert!(err.contains("Fail"), "{err}");
        assert!(
            mcu.dark_writes().is_empty(),
            "no switch write after a failed zone write"
        );

        let mut mcu = Mcu {
            dark: true,
            garble_report7: true,
            ..Mcu::default()
        };
        let err = set_zones(&mut mcu, 31, 1).unwrap_err().to_string();
        assert!(err.contains("does not echo"), "{err}");

        let mut mcu = Mcu {
            dark_data: Some(2),
            ..Mcu::default()
        };
        let err = status_report(&mut mcu).unwrap_err().to_string();
        assert!(err.contains("reads 2, expected 0 or 1"), "{err}");

        let mut mcu = Mcu {
            garble_sub2: true,
            ..Mcu::default()
        };
        let err = status_report(&mut mcu).unwrap_err().to_string();
        assert!(err.contains("does not echo"), "{err}");

        let mut mcu = Mcu {
            dark: true,
            stuck_dark: true,
            ..Mcu::default()
        };
        let err = set_zones(&mut mcu, 31, 1).unwrap_err().to_string();
        assert!(err.contains("still reads dark"), "{err}");
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
