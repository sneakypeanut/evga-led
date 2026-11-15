//! EVGA Z790 fan control over the Super I/O EC.
//!
//! Ported from the Python tools `fan/fan_ctl.py`, `fan/fan_probe.py` and
//! `fan/fan_sweep.py` (reverse engineered from ELEET X1 1.0.16).
//!
//! EC access: I/O ports 0x4E/0x4F, index register 0x2E, data register 0x2F.
//!
//! ```text
//! ECReg  Reg=0x10 Page=0x11 Data=0x12
//! ECPage AcpiRam=0xC4  SmartFan=0xC7
//!
//! 0xC4: FanControlPWMs@144 (2 B/fan: control, PWM)  FanSpeeds@160 (u16 LE RPM)
//!       FanMode@176 (u16, 2 bits/fan)
//! 0xC7: FanLevel@0(8) FanTolerance@8(8) Fans@16 (8 x [8 temps][8 pwms])
//!       Fan_Step_Modes@144(8) Current_PWM@152(8) Reserved@160(56) AdvancedMode@216
//! ```
//!
//! `FanControl` byte: bit 7 manual, bits 4-6 sensor source, bits 0-3 sensor index.
//! `AdvancedMode`: one bit per fan; 1 = use the custom curve above.

use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};

/// Name used in help output and error messages.
const TOOL: &str = "evga fan";

/// Raw port device, requires `CAP_SYS_RAWIO`.
pub const PORT_PATH: &str = "/dev/port";

const PORT_INDEX: u8 = 0x4E;
const PORT_DATA: u8 = 0x4F;
const INDEX_REG: u8 = 0x2E;
const DATA_REG: u8 = 0x2F;

const EREG_REG: u8 = 0x10;
const EREG_PAGE: u8 = 0x11;
const EREG_DATA: u8 = 0x12;

pub const PAGE_ACPI: u8 = 0xC4;
pub const PAGE_FAN: u8 = 0xC7;
/// Unused by this tool but part of the documented layout.
pub const PAGE_FAN_ROM: u8 = 0xD1;

/// Page size addressed by an EC register.
pub const PAGE_LEN: usize = 256;
/// Number of fan channels the EC exposes.
pub const FAN_COUNT: usize = 8;
/// Bytes per fan curve: 8 temperatures then 8 duties.
pub const CURVE_LEN: usize = 16;
/// Page 0xC7 offset of the per-fan curves.
pub const CURVE_BASE: usize = 16;
/// Page 0xC4 offset of `FanControlPWMs`.
pub const FAN_CONTROL_BASE: usize = 144;
/// Page 0xC4 offset of `FanSpeeds` (u16 LE per fan).
pub const FAN_SPEED_BASE: usize = 160;
/// Page 0xC7 offset of `Current_PWM`.
pub const CURRENT_PWM_BASE: usize = 152;
/// Page 0xC7 offset of `AdvancedMode`.
pub const ADVANCED_MODE: usize = 216;

/// RPM above this counts as spinning.
pub const MIN_SPIN_RPM: u16 = 200;

/// Duty below this is known to stall a fan on this board (measured 35%).
pub const STALL_FLOOR_PCT: u8 = 35;
/// Lowest low-end duty `quiet` is allowed to write.
pub const QUIET_MIN_PCT: u8 = 45;
/// Curves must be at 100% at or below this temperature.
pub const FULL_SPEED_TEMP_C: u8 = 70;

const QUIET_TEMPS: [u8; 8] = [0, 40, 50, 60, 70, 127, 127, 127];
/// Gentle low end, but never below the stall floor and 100% from 70 C on.
const QUIET_PWMS: [u8; 8] = [45, 55, 65, 80, 100, 100, 100, 100];

const SWEEP_DUTIES: [u8; 7] = [20, 25, 30, 35, 40, 45, 50];

/// Errors that are not I/O errors, carried as boxed `Error`.
#[derive(Debug)]
pub struct FanError(String);

impl FanError {
    fn new(message: impl Into<String>) -> Self {
        FanError(message.into())
    }
}

impl fmt::Display for FanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for FanError {}

fn fail<T>(message: impl Into<String>) -> io::Result<T> {
    Err(io::Error::new(io::ErrorKind::InvalidInput, message.into()))
}

// ---------------------------------------------------------------------------
// EC transport
// ---------------------------------------------------------------------------

/// Byte-level access to the EC's I/O ports. Implemented for real hardware by
/// [`PortBus`] and for tests by an in-memory model of the EC protocol.
pub trait EcBus {
    fn read_port(&mut self, port: u8) -> io::Result<u8>;
    fn write_port(&mut self, port: u8, value: u8) -> io::Result<()>;
}

/// `/dev/port` backed bus. `pread`/`pwrite` offsets are the port numbers.
pub struct PortBus {
    fd: RawFd,
}

impl PortBus {
    pub fn open(path: &Path) -> io::Result<Self> {
        let cpath = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("path contains a NUL byte: {}", path.display()),
            )
        })?;
        let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(open_error(path, io::Error::last_os_error()));
        }
        // Two tools interleaving register selections on one EC corrupt each
        // other's writes. Hold an exclusive, non-blocking lock for the lifetime
        // of the handle.
        if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let err = io::Error::last_os_error();
            unsafe {
                libc::close(fd);
            }
            return Err(io::Error::new(
                err.kind(),
                format!(
                    "{}: cannot lock the EC: {err} (another {TOOL} process is \
                     probably running; only one process may drive the EC)",
                    path.display()
                ),
            ));
        }
        Ok(PortBus { fd })
    }
}

fn open_error(path: &Path, err: io::Error) -> io::Error {
    let hint = match err.raw_os_error() {
        Some(libc::EACCES) | Some(libc::EPERM) => {
            format!("port I/O needs root or CAP_SYS_RAWIO; run {TOOL} through pkexec")
        }
        Some(libc::ENOENT) => {
            "this kernel has no /dev/port; the raw port driver is missing".to_string()
        }
        _ => "cannot open the EC port".to_string(),
    };
    io::Error::new(err.kind(), format!("{}: {}: {hint}", path.display(), err))
}

impl Drop for PortBus {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.fd);
        }
    }
}

impl EcBus for PortBus {
    fn read_port(&mut self, port: u8) -> io::Result<u8> {
        let mut byte = [0u8; 1];
        let want = 1usize;
        let n = unsafe {
            libc::pread(
                self.fd,
                byte.as_mut_ptr().cast::<libc::c_void>(),
                want,
                libc::off_t::from(port),
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if n != want as libc::ssize_t {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("port 0x{port:02X}: short read"),
            ));
        }
        Ok(byte[0])
    }

    fn write_port(&mut self, port: u8, value: u8) -> io::Result<()> {
        let byte = [value];
        let n = unsafe {
            libc::pwrite(
                self.fd,
                byte.as_ptr().cast::<libc::c_void>(),
                1,
                libc::off_t::from(port),
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if n != 1 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                format!("port 0x{port:02X}: short write"),
            ));
        }
        Ok(())
    }
}

/// EC register-window protocol on top of a [`EcBus`].
pub struct Ec<B: EcBus> {
    bus: B,
}

impl<B: EcBus> Ec<B> {
    pub fn new(bus: B) -> Self {
        Ec { bus }
    }

    /// Select `ecreg`, then either read its data byte or write `value` to it.
    fn select(&mut self, ecreg: u8, value: Option<u8>) -> io::Result<u8> {
        self.bus.write_port(PORT_INDEX, INDEX_REG)?;
        self.bus.write_port(PORT_DATA, ecreg)?;
        self.bus.write_port(PORT_INDEX, DATA_REG)?;
        match value {
            None => self.bus.read_port(PORT_DATA),
            Some(v) => {
                self.bus.write_port(PORT_DATA, v)?;
                Ok(0)
            }
        }
    }

    pub fn read(&mut self, page: u8, reg: u8) -> io::Result<u8> {
        self.select(EREG_PAGE, Some(page))?;
        self.select(EREG_REG, Some(reg))?;
        self.select(EREG_DATA, None)
    }

    pub fn write(&mut self, page: u8, reg: u8, value: u8) -> io::Result<()> {
        self.select(EREG_PAGE, Some(page))?;
        self.select(EREG_REG, Some(reg))?;
        self.select(EREG_DATA, Some(value))?;
        Ok(())
    }

    pub fn dump(&mut self, page: u8) -> io::Result<Vec<u8>> {
        read_block(self, page, 0, PAGE_LEN)
    }

    /// Borrow the underlying bus (used by tests and diagnostics).
    pub fn bus(&self) -> &B {
        &self.bus
    }
}

/// VendorID the EVGA Z790 EC reports at page 0xC4 offset 0.
pub const EVGA_VENDOR_ID: u16 = 0x3842;

/// Refuse to configure an EC that does not identify as the EVGA Z790 one.
fn require_evga_ec(state: &EcState) -> io::Result<()> {
    if state.acpi.len() < 2 {
        return fail("page 0xC4 is too short to read VendorID");
    }
    let vendor = u16::from(state.acpi[0]) | (u16::from(state.acpi[1]) << 8);
    if vendor != EVGA_VENDOR_ID {
        return fail(format!(
            "refusing to write: VendorID at 0xC4:0 is 0x{vendor:04X}, not the EVGA Z790 EC \
             (0x{EVGA_VENDOR_ID:04X}); run `{TOOL} probe` to inspect this machine"
        ));
    }
    Ok(())
}

fn read_block<B: EcBus>(ec: &mut Ec<B>, page: u8, start: usize, len: usize) -> io::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(len);
    for offset in 0..len {
        out.push(ec.read(page, (start + offset) as u8)?);
    }
    Ok(out)
}

/// Open the real hardware EC at [`PORT_PATH`].
pub fn open_hardware() -> io::Result<Ec<PortBus>> {
    Ok(Ec::new(PortBus::open(Path::new(PORT_PATH))?))
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Snapshot of both EC pages, written to and read from the backup JSON.
///
/// Field names and shapes match the Python tools exactly, so old backups
/// (`~/.local/state/evga-fan/backup-*.json`) remain restorable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EcState {
    pub acpi: Vec<u8>,
    pub smartfan: Vec<u8>,
    #[serde(default)]
    pub rpm: Vec<u16>,
    #[serde(default)]
    pub pwm_now: Vec<u8>,
    #[serde(default)]
    pub advanced: u8,
    #[serde(default)]
    pub curves: Vec<Vec<u8>>,
}

impl EcState {
    fn curve(&self, fan: usize) -> &[u8] {
        self.curves
            .get(fan)
            .map(Vec::as_slice)
            .unwrap_or(&[0u8; CURVE_LEN])
    }
}

fn read_all<B: EcBus>(ec: &mut Ec<B>) -> io::Result<EcState> {
    let acpi = ec.dump(PAGE_ACPI)?;
    let smartfan = ec.dump(PAGE_FAN)?;

    let mut rpm = Vec::with_capacity(FAN_COUNT);
    let mut pwm_now = Vec::with_capacity(FAN_COUNT);
    let mut curves = Vec::with_capacity(FAN_COUNT);
    for fan in 0..FAN_COUNT {
        let lo = acpi[FAN_SPEED_BASE + fan * 2];
        let hi = acpi[FAN_SPEED_BASE + fan * 2 + 1];
        rpm.push(u16::from(lo) | (u16::from(hi) << 8));
        pwm_now.push(smartfan[CURRENT_PWM_BASE + fan]);
        curves.push(
            smartfan[CURVE_BASE + fan * CURVE_LEN..CURVE_BASE + (fan + 1) * CURVE_LEN].to_vec(),
        );
    }

    let advanced = smartfan[ADVANCED_MODE];
    Ok(EcState {
        acpi,
        smartfan,
        rpm,
        pwm_now,
        advanced,
        curves,
    })
}

fn show<B: EcBus>(ec: &mut Ec<B>) -> io::Result<EcState> {
    let state = read_all(ec)?;
    println!(
        "{:>3} {:>6} {:>5} {:>4}  temps -> pwms",
        "fan", "RPM", "PWM%", "adv"
    );
    for fan in 0..FAN_COUNT {
        let curve = state.curve(fan);
        let adv = (state.advanced >> fan) & 1;
        println!(
            "{:>3} {:>6} {:>5} {:>4}  {:?} -> {:?}",
            fan,
            state.rpm.get(fan).copied().unwrap_or(0),
            state.pwm_now.get(fan).copied().unwrap_or(0),
            adv,
            &curve[..6],
            &curve[8..14]
        );
    }
    Ok(state)
}

// ---------------------------------------------------------------------------
// Curve validation and writing
// ---------------------------------------------------------------------------

/// A fan's 8 temperatures and 8 duties, already range-checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Curve {
    pub temps: [u8; 8],
    pub pwms: [u8; 8],
}

impl Curve {
    fn from_slices(temps: &[u8; 8], pwms: &[u8; 8]) -> Self {
        Curve {
            temps: *temps,
            pwms: *pwms,
        }
    }
}

fn validate_fan_index(fan: i64) -> std::result::Result<usize, FanError> {
    if !(0..FAN_COUNT as i64).contains(&fan) {
        return Err(FanError::new(format!(
            "fan index {fan} out of range: {TOOL} controls fans 0..{}",
            FAN_COUNT - 1
        )));
    }
    Ok(fan as usize)
}

/// Range and ordering checks shared by the CLI and the sweep.
fn validate_curve(curve: &Curve, min_low_pwm: u8) -> std::result::Result<(), FanError> {
    for (i, &t) in curve.temps.iter().enumerate() {
        if t > 127 {
            return Err(FanError::new(format!(
                "temperature point {i} is {t} C; the EC accepts 0..=127"
            )));
        }
        if i > 0 && t < curve.temps[i - 1] {
            return Err(FanError::new(format!(
                "temperatures must not decrease: point {i} ({t} C) < point {} ({} C)",
                i - 1,
                curve.temps[i - 1]
            )));
        }
    }

    for (i, &p) in curve.pwms.iter().enumerate() {
        if p > 100 {
            return Err(FanError::new(format!(
                "duty point {i} is {p}%; use 0..=100 (percent)"
            )));
        }
        if i > 0 && p < curve.pwms[i - 1] {
            return Err(FanError::new(format!(
                "duties must not decrease: point {i} ({p}%) < point {} ({}%)",
                i - 1,
                curve.pwms[i - 1]
            )));
        }
    }

    if curve.pwms[0] < min_low_pwm {
        return Err(FanError::new(format!(
            "lowest duty {}% is below the {}% floor; fans on this board stall below {}% \
             (run `{TOOL} sweep` to find the real floor for this fan)",
            curve.pwms[0], min_low_pwm, STALL_FLOOR_PCT
        )));
    }

    // 100% must be reached at or below FULL_SPEED_TEMP_C. Checking the first
    // 100% point is enough because duties are already proven non-decreasing
    // above, so every later point is 100% too. Requiring only "some point at
    // or above 70 C is 100%" would accept a curve that stays at 80% up to
    // 80 C.
    let first_full = curve.pwms.iter().position(|&p| p == 100).ok_or_else(|| {
        FanError::new(format!(
            "curve never commands 100%; it must reach 100% by {FULL_SPEED_TEMP_C} C"
        ))
    })?;
    if curve.temps[first_full] > FULL_SPEED_TEMP_C {
        return Err(FanError::new(format!(
            "curve first commands 100% at {C} C (point {first_full}), which is above the \
             {FULL_SPEED_TEMP_C} C limit; only soften the low end",
            C = curve.temps[first_full]
        )));
    }
    Ok(())
}

/// Parse 16 CLI values into a validated [`Curve`].
fn curve_from_values(values: &[i64], min_low_pwm: u8) -> std::result::Result<Curve, FanError> {
    if values.len() != CURVE_LEN {
        return Err(FanError::new(format!(
            "need {CURVE_LEN} values (8 temperatures then 8 duties), got {}",
            values.len()
        )));
    }
    let mut temps = [0u8; 8];
    let mut pwms = [0u8; 8];
    for (i, &raw) in values.iter().enumerate() {
        if !(0..=255).contains(&raw) {
            return Err(FanError::new(format!("value {raw} out of byte range")));
        }
        if i < 8 {
            temps[i] = raw as u8;
        } else {
            pwms[i - 8] = raw as u8;
        }
    }
    let curve = Curve::from_slices(&temps, &pwms);
    validate_curve(&curve, min_low_pwm)?;
    Ok(curve)
}

/// Write one fan's curve and set or clear its `AdvancedMode` bit.
///
/// The caller is responsible for having taken a backup first.
fn write_curve<B: EcBus>(
    ec: &mut Ec<B>,
    fan: usize,
    curve: &Curve,
    advanced: bool,
) -> io::Result<()> {
    let base = CURVE_BASE + fan * CURVE_LEN;
    for (i, &t) in curve.temps.iter().enumerate() {
        ec.write(PAGE_FAN, (base + i) as u8, t)?;
    }
    for (i, &p) in curve.pwms.iter().enumerate() {
        ec.write(PAGE_FAN, (base + 8 + i) as u8, p)?;
    }
    let mask = 1u8 << fan;
    let current = ec.read(PAGE_FAN, ADVANCED_MODE as u8)?;
    let next = if advanced {
        current | mask
    } else {
        current & !mask
    };
    if next != current {
        ec.write(PAGE_FAN, ADVANCED_MODE as u8, next)?;
    }
    Ok(())
}

/// Best-effort read-back of a just-written fan.
fn verify_curve<B: EcBus>(ec: &mut Ec<B>, fan: usize, curve: &Curve) {
    let base = CURVE_BASE + fan * CURVE_LEN;
    for (i, &expected) in curve.temps.iter().chain(curve.pwms.iter()).enumerate() {
        match ec.read(PAGE_FAN, (base + i) as u8) {
            Ok(actual) if actual == expected => {}
            Ok(actual) => {
                eprintln!(
                    "warning: fan {fan} curve byte {i} reads back {actual}, expected {expected}"
                );
                return;
            }
            Err(err) => {
                eprintln!("warning: fan {fan} curve read-back failed: {err}");
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Backups
// ---------------------------------------------------------------------------

/// State directory: `$EVGA_FAN_STATE` or `~/.local/state/evga-fan`.
pub fn state_dir() -> io::Result<PathBuf> {
    if let Some(dir) = std::env::var_os("EVGA_FAN_STATE").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "HOME is unset; set HOME or EVGA_FAN_STATE",
            )
        })?;
    Ok(PathBuf::from(home).join(".local/state/evga-fan"))
}

/// `%Y%m%d-%H%M%S` in local time, matching the Python tool's file names.
fn local_timestamp() -> String {
    let now = unsafe { libc::time(std::ptr::null_mut()) };
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::localtime_r(&now, &mut tm) };
    if ok.is_null() {
        return now.to_string();
    }
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

/// Write a backup before any EC write. Returns the created path.
///
/// Uses `create_new` so an existing file or a symlink placed at the target name
/// is never followed or overwritten, and fsyncs the bytes before returning.
fn write_backup(dir: &Path, prefix: &str, state: &EcState) -> io::Result<PathBuf> {
    use std::io::Write;

    fs::create_dir_all(dir)?;
    let stamp = local_timestamp();
    let body = serde_json::to_vec_pretty(state).map_err(|err| io::Error::other(err.to_string()))?;

    let mut n = 0u32;
    loop {
        let name = if n == 0 {
            format!("{prefix}-{stamp}.json")
        } else {
            format!("{prefix}-{stamp}-{n}.json")
        };
        let path = dir.join(name);
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                file.write_all(&body)?;
                file.sync_all()?;
                return Ok(path);
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                n += 1;
                if n > 10_000 {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!("{}: too many backups with stamp {stamp}", dir.display()),
                    ));
                }
            }
            Err(err) => return Err(err),
        }
    }
}

/// Default restore source: the `backup-*.json` with the highest
/// `(stamp, collision-suffix)`, which is the order the tool writes them in.
/// mtime is not used, so copying or touching an old backup cannot make it win.
fn latest_backup(dir: &Path) -> io::Result<PathBuf> {
    let mut best: Option<((String, u32), PathBuf)> = None;
    let entries = fs::read_dir(dir).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("no backups in {}: {err}", dir.display()),
        )
    })?;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(rest) = name.strip_prefix("backup-") else {
            continue;
        };
        let Some(rest) = rest.strip_suffix(".json") else {
            continue;
        };
        // Written as `backup-{YYYYMMDD-HHMMSS}[-{n}].json`.
        let key = backup_sort_key(rest);
        if best.as_ref().map(|(k, _)| key >= *k).unwrap_or(true) {
            best = Some((key, entry.path()));
        }
    }
    best.map(|(_, path)| path).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "no backup-*.json in {}; run `{TOOL} backup` first",
                dir.display()
            ),
        )
    })
}

/// Parse `YYYYMMDD-HHMMSS` with an optional numeric collision suffix. Names
/// that are not timestamped (e.g. `backup-before-bios.json`) sort below every
/// real backup instead of above them because `b` > `2`.
fn backup_sort_key(stem: &str) -> (String, u32) {
    let bytes = stem.as_bytes();
    let timestamped = bytes.len() >= 15
        && bytes[..8].iter().all(u8::is_ascii_digit)
        && bytes[8] == b'-'
        && bytes[9..15].iter().all(u8::is_ascii_digit);
    if !timestamped {
        return (String::new(), 0);
    }
    let (stamp, suffix) = stem.split_at(15);
    let suffix = suffix.strip_prefix('-').unwrap_or(suffix);
    let n = suffix.parse::<u32>().unwrap_or(0);
    (stamp.to_string(), n)
}

fn load_state(path: &Path) -> io::Result<EcState> {
    let raw = fs::read(path)?;
    let state: EcState = serde_json::from_slice(&raw).map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is not a valid EC backup: {err}", path.display()),
        )
    })?;
    // A restore writes both pages verbatim, so a short page would leave part of
    // the EC in an unknown state. Require the full 256 bytes for both.
    for (page, bytes) in [("acpi", &state.acpi), ("smartfan", &state.smartfan)] {
        if bytes.len() != PAGE_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{}: page '{page}' is {} bytes, expected exactly {PAGE_LEN}",
                    path.display(),
                    bytes.len()
                ),
            ));
        }
    }
    Ok(state)
}

fn restore_pages<B: EcBus>(ec: &mut Ec<B>, state: &EcState) -> io::Result<()> {
    for (reg, &value) in state.acpi.iter().enumerate() {
        ec.write(PAGE_ACPI, reg as u8, value)?;
    }
    for (reg, &value) in state.smartfan.iter().enumerate() {
        ec.write(PAGE_FAN, reg as u8, value)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Debug, Args)]
#[command(
    about = "EVGA Z790 fan control via the Super I/O EC",
    long_about = "EVGA Z790 fan control via the Super I/O EC.\n\n\
                  Writes raw I/O ports and needs root or CAP_SYS_RAWIO. Backs up both EC \
                  pages before any write. `quiet` never writes a low-end duty below 45%, \
                  `curve` refuses anything below the measured 35% stall floor, and every \
                  curve must reach 100% by 70 C. The sweep tool probes 20-50% on purpose, \
                  then settles at 45% or more."
)]
pub struct Cli {
    #[command(subcommand)]
    pub action: Action,
}

#[derive(Debug, Subcommand)]
pub enum Action {
    /// Print RPM, live duty and curves for all fans
    Show,
    /// Dump both EC pages to the state directory
    Backup,
    /// Write gentle curves (never below the stall floor)
    Quiet,
    /// Write one fan's 8 temperatures then 8 duties
    Curve {
        /// Fan index, 0..=7
        fan: i64,
        /// t0..t7 (C, 0..=127) then p0..p7 (%, 0..=100)
        #[arg(value_name = "VAL", num_args = CURVE_LEN, required = true)]
        vals: Vec<i64>,
    },
    /// Write both EC pages back from a backup file
    Restore {
        /// Backup JSON, defaults to the newest backup-*.json
        file: Option<PathBuf>,
    },
    /// Read-only dump of EC IDs, RPM, duty and curves
    Probe {
        /// Raw port device to read
        #[arg(long, default_value = PORT_PATH, value_name = "PATH")]
        device: PathBuf,
    },
    /// Find a fan's stall floor, then settle a safe curve
    Sweep {
        /// Fan index to sweep, 0..=7
        #[arg(default_value_t = 7)]
        fan: i64,
    },
}

#[derive(Debug, PartialEq, Eq)]
enum Plan {
    Show,
    Backup,
    Quiet,
    Curve { fan: usize, curve: Curve },
    Restore { path: PathBuf, state: EcState },
}

/// Validate and resolve everything that does not need hardware, so a bad
/// request never opens `/dev/port` or writes to the EC.
fn plan_action(args: &Cli, dir: &Path) -> std::result::Result<Plan, FanError> {
    Ok(match &args.action {
        Action::Show => Plan::Show,
        Action::Backup => Plan::Backup,
        Action::Quiet => Plan::Quiet,
        Action::Curve { fan, vals } => {
            let fan = validate_fan_index(*fan)?;
            let curve = curve_from_values(vals, STALL_FLOOR_PCT)?;
            Plan::Curve { fan, curve }
        }
        Action::Restore { file } => {
            let path = match file {
                Some(path) => path.clone(),
                None => latest_backup(dir).map_err(FanError::from)?,
            };
            let state = load_state(&path).map_err(FanError::from)?;
            // A 256-byte page from another board (or a corrupted file) would be
            // written verbatim, including FanControlPWMs. Only accept backups
            // that identify as this board's EC.
            require_evga_ec(&state).map_err(FanError::from)?;
            Plan::Restore { path, state }
        }
        // Handled by `run` before planning; neither goes through a Plan.
        Action::Probe { .. } | Action::Sweep { .. } => {
            return Err(FanError::new(
                "internal error: probe and sweep are dispatched before planning",
            ));
        }
    })
}

impl From<io::Error> for FanError {
    fn from(err: io::Error) -> Self {
        FanError(err.to_string())
    }
}

/// Entry point for `evga fan ...`: probe and sweep first (they own their own
/// hardware access), everything else through the validated `run_with` path.
/// `state_dir` is only resolved for commands that use backups, so probe still
/// works with HOME unset.
pub fn run(args: Cli) -> Result<(), Box<dyn Error>> {
    match &args.action {
        Action::Probe { device } => {
            let mut ec = Ec::new(PortBus::open(device)?);
            probe(&mut ec)?;
            Ok(())
        }
        Action::Sweep { fan } => {
            let dir = state_dir()?;
            sweep_run_with(*fan, &dir, open_hardware)
        }
        _ => {
            let dir = state_dir()?;
            run_with(args, &dir, open_hardware)
        }
    }
}

fn run_with<B, F>(args: Cli, dir: &Path, open: F) -> Result<(), Box<dyn Error>>
where
    B: EcBus,
    F: FnOnce() -> io::Result<Ec<B>>,
{
    let plan = plan_action(&args, dir)?;
    let mut ec = open()?;
    execute(plan, &mut ec, dir)?;
    Ok(())
}

fn execute<B: EcBus>(plan: Plan, ec: &mut Ec<B>, dir: &Path) -> Result<(), Box<dyn Error>> {
    match plan {
        Plan::Show => {
            show(ec)?;
        }
        Plan::Backup => {
            let state = read_all(ec)?;
            let path = write_backup(dir, "backup", &state)?;
            println!("saved {}", path.display());
        }
        Plan::Quiet => quiet_with(ec, dir, Timing::default())?,
        Plan::Curve { fan, curve } => {
            let state = read_all(ec)?;
            require_evga_ec(&state)?;
            let path = write_backup(dir, "pre-curve", &state)?;
            println!("backup: {}", path.display());
            write_curve(ec, fan, &curve, true)?;
            verify_curve(ec, fan, &curve);
            println!("fan {fan} curve written");
        }
        Plan::Restore { path, state } => {
            let current = read_all(ec)?;
            require_evga_ec(&current)?;
            let backup = write_backup(dir, "pre-restore", &current)?;
            println!("backup: {}", backup.display());
            restore_pages(ec, &state)?;
            println!("restored both pages from {}", path.display());
        }
    }
    Ok(())
}

/// Delays used between EC writes and reads. Tests use zeros.
#[derive(Debug, Clone, Copy)]
struct Timing {
    /// Wait for the EC to re-evaluate after `quiet`.
    quiet_settle: Duration,
    /// Wait per sweep step.
    sweep_step: Duration,
    /// Wait after writing the settled sweep curve.
    sweep_settle: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Timing {
            quiet_settle: Duration::from_secs(10),
            sweep_step: Duration::from_secs(5),
            sweep_settle: Duration::from_secs(6),
        }
    }
}

fn quiet_with<B: EcBus>(ec: &mut Ec<B>, dir: &Path, timing: Timing) -> io::Result<()> {
    let state = read_all(ec)?;
    require_evga_ec(&state)?;
    let path = write_backup(dir, "pre-quiet", &state)?;
    println!("backup: {}", path.display());

    for fan in 0..FAN_COUNT {
        if state.rpm.get(fan).copied().unwrap_or(0) == 0 {
            println!("fan {fan}: no tach, skipped");
            continue;
        }
        let curve = Curve::from_slices(&QUIET_TEMPS, &QUIET_PWMS);
        if let Err(err) = validate_curve(&curve, QUIET_MIN_PCT) {
            // Unreachable with the constants above, but never write an
            // unvalidated curve.
            eprintln!("fan {fan}: refusing quiet curve: {err}");
            continue;
        }
        write_curve(ec, fan, &curve, true)?;
        println!("fan {fan}: curve -> {:?} + advanced mode", QUIET_PWMS);
    }

    println!(
        "\nwaiting {} s for the EC to re-evaluate...",
        timing.quiet_settle.as_secs()
    );
    std::thread::sleep(timing.quiet_settle);
    show(ec)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Read-only probe
// ---------------------------------------------------------------------------

fn probe<B: EcBus>(ec: &mut Ec<B>) -> io::Result<()> {
    let ident = read_block(ec, PAGE_ACPI, 0, 10)?;
    let vendor = u16::from(ident[0]) | (u16::from(ident[1]) << 8);
    let device = u16::from(ident[2]) | (u16::from(ident[3]) << 8);
    println!(
        "AcpiRam page: VendorID=0x{vendor:04X} DeviceID=0x{device:04X} \
         main={} sub={} adc={}/{} fan={}/{}",
        ident[4], ident[5], ident[6], ident[7], ident[8], ident[9]
    );
    if vendor == 0xFFFF || (vendor == 0 && device == 0) {
        println!("  -> no valid EC data at these ports (expect 0x3842 for EVGA)");
    }

    println!("\n--- per-fan state (page 0x{PAGE_ACPI:02X}) ---");
    let speeds = read_block(ec, PAGE_ACPI, FAN_SPEED_BASE, FAN_COUNT * 2)?;
    let pwmcfg = read_block(ec, PAGE_ACPI, FAN_CONTROL_BASE, FAN_COUNT * 2)?;
    println!("{:>3} {:>8} {:>5} {:>6}", "fan", "control", "PWM", "RPM");
    for fan in 0..FAN_COUNT {
        let rpm = u16::from(speeds[fan * 2]) | (u16::from(speeds[fan * 2 + 1]) << 8);
        let control = pwmcfg[fan * 2];
        let pwm = pwmcfg[fan * 2 + 1];
        println!("{fan:>3} {control:>8} {pwm:>5} {rpm:>6}");
    }
    let fanmode = read_block(ec, PAGE_ACPI, 176, 2)?;
    println!(
        "FanMode = {}",
        u16::from(fanmode[0]) | (u16::from(fanmode[1]) << 8)
    );

    println!("\n--- curves (page 0x{PAGE_FAN:02X}) ---");
    let adv = ec.read(PAGE_FAN, ADVANCED_MODE as u8)?;
    let cur_pwm = read_block(ec, PAGE_FAN, CURRENT_PWM_BASE, FAN_COUNT)?;
    let step = read_block(ec, PAGE_FAN, 144, FAN_COUNT)?;
    let levels = read_block(ec, PAGE_FAN, 0, FAN_COUNT)?;
    let tol = read_block(ec, PAGE_FAN, 8, FAN_COUNT)?;
    println!("AdvancedMode={adv}  Current_PWM={cur_pwm:?}  StepModes={step:?}");
    println!("FanLevel={levels:?}  FanTolerance={tol:?}");
    for fan in 0..FAN_COUNT {
        let block = read_block(ec, PAGE_FAN, CURVE_BASE + fan * CURVE_LEN, CURVE_LEN)?;
        println!(
            "  fan {fan}: temps={:?} pwms={:?}",
            &block[..8],
            &block[8..]
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Sweep
// ---------------------------------------------------------------------------

/// Validate the fan index, then open hardware and sweep.
fn sweep_run_with<B, F>(fan: i64, dir: &Path, open: F) -> Result<(), Box<dyn Error>>
where
    B: EcBus,
    F: FnOnce() -> io::Result<Ec<B>>,
{
    let fan = validate_fan_index(fan)?;
    let mut ec = open()?;
    sweep_with(&mut ec, fan, dir, Timing::default(), true)
}

fn sweep_with<B: EcBus>(
    ec: &mut Ec<B>,
    fan: usize,
    dir: &Path,
    timing: Timing,
    verbose: bool,
) -> Result<(), Box<dyn Error>> {
    // Back up before the first write, so any failure can be undone.
    let before = read_all(ec)?;
    require_evga_ec(&before)?;
    let backup = write_backup(dir, "pre-sweep", &before)?;
    if verbose {
        println!("fan {fan}: backup {}", backup.display());
    }
    println!(
        "fan {fan}: interrupting the sweep leaves a probe curve; run `{TOOL} restore {}` to undo",
        backup.display()
    );
    let original_curve = curve_from_state(&before, fan)?;
    let original_advanced = (before.advanced >> fan) & 1 == 1;

    let result = sweep_steps(ec, fan, timing, verbose);

    match result {
        Ok(()) => Ok(()),
        Err(err) => {
            // Put the fan back the way it was, then report both outcomes.
            let restore = write_curve(ec, fan, &original_curve, original_advanced);
            let readback = read_all(ec).map(|s| s.rpm.get(fan).copied().unwrap_or(0));
            match (restore, readback) {
                (Ok(()), Ok(rpm)) => eprintln!(
                    "sweep failed ({err}); restored fan {fan} from {} (now {rpm} RPM)",
                    backup.display()
                ),
                (Ok(()), Err(read_err)) => eprintln!(
                    "sweep failed ({err}); restored fan {fan} from {} but could not read back \
                     RPM: {read_err}",
                    backup.display()
                ),
                (Err(restore_err), _) => eprintln!(
                    "sweep failed ({err}); restoring fan {fan} also failed: {restore_err}. \
                     Run `{TOOL} restore {}`",
                    backup.display()
                ),
            }
            Err(err.into())
        }
    }
}

fn curve_from_state(state: &EcState, fan: usize) -> std::result::Result<Curve, FanError> {
    let bytes = state
        .curves
        .get(fan)
        .ok_or_else(|| FanError::new(format!("backup has no curve for fan {fan}")))?;
    if bytes.len() != CURVE_LEN {
        return Err(FanError::new(format!(
            "backup curve for fan {fan} is {} bytes, expected {CURVE_LEN}",
            bytes.len()
        )));
    }
    let mut temps = [0u8; 8];
    let mut pwms = [0u8; 8];
    temps.copy_from_slice(&bytes[..8]);
    pwms.copy_from_slice(&bytes[8..]);
    Ok(Curve { temps, pwms })
}

fn sweep_steps<B: EcBus>(
    ec: &mut Ec<B>,
    fan: usize,
    timing: Timing,
    verbose: bool,
) -> io::Result<()> {
    let temps = QUIET_TEMPS;
    if verbose {
        println!("fan {fan}: sweeping duty to find the stall floor");
        println!("{:>5} {:>8} {:>6}", "duty", "measured", "RPM");
    }
    let mut floor: Option<(u8, u8)> = None;
    for duty in SWEEP_DUTIES {
        let pwms = [duty, duty + 5, duty + 15, duty + 30, 100, 100, 100, 100];
        let curve = Curve::from_slices(&temps, &pwms);
        // Probing deliberately goes below the stall floor; only the range and
        // 100%-by-70C rules are enforceable here.
        validate_curve(&curve, 0)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err.to_string()))?;
        write_curve(ec, fan, &curve, true)?;
        std::thread::sleep(timing.sweep_step);
        let state = read_all(ec)?;
        let rpm = state.rpm.get(fan).copied().unwrap_or(0);
        // The EC picks the duty for the current sensor temperature, so the
        // requested low point is not necessarily what the fan is driven with.
        // Current_PWM is the duty the EC actually applied.
        let measured = state.pwm_now.get(fan).copied().unwrap_or(duty);
        if measured > 100 {
            // The EC register is documented only as "PWM%"; if a board reports
            // raw 0..255 the margin maths would pin the fan at full speed. Fail
            // so the caller restores the original curve instead.
            return Err(io::Error::other(format!(
                "fan {fan}: Current_PWM reports {measured}, outside 0..=100; the register unit \
                 is unverified, so not sweeping. Run `{TOOL} probe` and check Current_PWM"
            )));
        }
        if verbose {
            println!("{duty:>5} {measured:>8} {rpm:>6}");
        }
        if rpm > MIN_SPIN_RPM {
            floor = Some((duty, measured));
            break;
        }
    }

    let (duty, measured) = match floor {
        Some(found) => found,
        None => {
            // Settling at 45% without ever seeing the fan turn would report a
            // "safe floor" that was never measured, and could leave a stalled
            // fan with no cooling. Fail so the caller restores the original
            // curve instead.
            return Err(io::Error::other(format!(
                "fan {fan}: no duty in {SWEEP_DUTIES:?}% produced more than {MIN_SPIN_RPM} RPM; \
                 not writing a curve that could leave the fan stalled"
            )));
        }
    };
    if verbose {
        println!("\nfirst duty that spins: requested {duty}%, measured PWM {measured}%");
    }

    // Margin above the duty the EC actually applied, never below the 45% floor.
    // Points are clamped at 100% instead of dropping margin, and the 85% point
    // follows the previous point so duties stay non-decreasing.
    let base = (u16::from(measured) + 10).clamp(45, 100) as u8;
    let p1 = base.saturating_add(10).min(100);
    let p2 = base.saturating_add(25).min(100);
    let pwms = [base, p1, p2, 85.max(p2), 100, 100, 100, 100];
    let curve = Curve::from_slices(&temps, &pwms);
    validate_curve(&curve, QUIET_MIN_PCT)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err.to_string()))?;
    write_curve(ec, fan, &curve, true)?;
    std::thread::sleep(timing.sweep_settle);
    let state = read_all(ec)?;
    let rpm = state.rpm.get(fan).copied().unwrap_or(0);
    if verbose {
        println!("settled curve: temps={temps:?}");
        println!("               pwms ={pwms:?}  -> RPM={rpm}");
    }
    if rpm <= MIN_SPIN_RPM {
        // The curve that measured as spinning did not keep the fan turning
        // after settling. Treat that as a failure so the original curve is put
        // back rather than leaving the fan stalled.
        return Err(io::Error::other(format!(
            "fan {fan}: settled curve left the fan at {rpm} RPM (<= {MIN_SPIN_RPM}); \
             restoring the original curve"
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests: in-memory EC, no hardware touched
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    /// In-memory model of the EC register window. Implements the real
    /// 0x4E/0x4F + 0x2E/0x2F protocol, so the code under test is exercised
    /// through the same byte sequence the hardware sees.
    #[derive(Debug, Default, Clone)]
    struct MockEc {
        last_index: u8,
        ecreg: u8,
        selected_reg: u8,
        selected_page: u8,
        pages: HashMap<u8, [u8; PAGE_LEN]>,
        writes: Vec<(u8, u8, u8)>,
        fail_once_at: Option<usize>,
        fail_all: bool,
        write_count: usize,
        reads: usize,
        /// Optional model of a fan whose tach reads 0 RPM below `floor` and
        /// 1200 RPM at or above it, driven by the curve's low-end duty.
        tach_floor: Option<(usize, u8)>,
        /// Degrees the modelled EC adds to a written low point when it reports
        /// `Current_PWM`, standing in for a warmer sensor.
        pwm_bias: u8,
    }

    impl MockEc {
        fn seed(&mut self, page: u8, reg: usize, value: u8) {
            self.pages.entry(page).or_insert([0u8; PAGE_LEN])[reg] = value;
        }

        fn page(&self, page: u8) -> [u8; PAGE_LEN] {
            self.pages.get(&page).copied().unwrap_or([0u8; PAGE_LEN])
        }

        fn writes_to(&self, page: u8, reg: usize) -> Vec<u8> {
            self.writes
                .iter()
                .filter(|(p, r, _)| *p == page && *r as usize == reg)
                .map(|(_, _, v)| *v)
                .collect()
        }
    }

    impl EcBus for MockEc {
        fn read_port(&mut self, port: u8) -> io::Result<u8> {
            self.reads += 1;
            if port == PORT_DATA && self.last_index == DATA_REG && self.ecreg == EREG_DATA {
                if let Some((fan, floor)) = self.tach_floor {
                    let base = FAN_SPEED_BASE + fan * 2;
                    let reg = self.selected_reg as usize;
                    if self.selected_page == PAGE_ACPI && (reg == base || reg == base + 1) {
                        let low = self.page(PAGE_FAN)[CURVE_BASE + fan * CURVE_LEN + 8];
                        let rpm: u16 = if low >= floor { 1200 } else { 0 };
                        return Ok(if reg == base {
                            (rpm & 0xFF) as u8
                        } else {
                            (rpm >> 8) as u8
                        });
                    }
                }
                return Ok(self.page(self.selected_page)[self.selected_reg as usize]);
            }
            Ok(0)
        }

        fn write_port(&mut self, port: u8, value: u8) -> io::Result<()> {
            match port {
                PORT_INDEX => self.last_index = value,
                PORT_DATA => {
                    if self.last_index == INDEX_REG {
                        self.ecreg = value;
                        return Ok(());
                    }
                    match self.ecreg {
                        EREG_REG => self.selected_reg = value,
                        EREG_PAGE => self.selected_page = value,
                        EREG_DATA => {
                            if self.fail_all {
                                return Err(io::Error::new(
                                    io::ErrorKind::BrokenPipe,
                                    "injected EC write fault",
                                ));
                            }
                            if self.fail_once_at == Some(self.write_count) {
                                self.fail_once_at = None;
                                return Err(io::Error::new(
                                    io::ErrorKind::BrokenPipe,
                                    "injected EC write fault",
                                ));
                            }
                            self.pages
                                .entry(self.selected_page)
                                .or_insert([0u8; PAGE_LEN])
                                [self.selected_reg as usize] = value;
                            // The EC reflects the low curve point in Current_PWM;
                            // `pwm_bias` models a warmer sensor asking for more.
                            if self.selected_page == PAGE_FAN {
                                let reg = self.selected_reg as usize;
                                let offset = reg.wrapping_sub(CURVE_BASE);
                                if reg >= CURVE_BASE
                                    && offset < FAN_COUNT * CURVE_LEN
                                    && offset % CURVE_LEN == 8
                                {
                                    let fan = offset / CURVE_LEN;
                                    let driven = value.saturating_add(self.pwm_bias);
                                    self.pages.entry(PAGE_FAN).or_insert([0u8; PAGE_LEN])
                                        [CURRENT_PWM_BASE + fan] = driven;
                                }
                            }
                            self.writes
                                .push((self.selected_page, self.selected_reg, value));
                            self.write_count += 1;
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
            Ok(())
        }
    }

    fn seeded_mock() -> MockEc {
        let mut mock = MockEc::default();
        // VendorID 0x3842 on page 0xC4.
        mock.seed(PAGE_ACPI, 0, 0x42);
        mock.seed(PAGE_ACPI, 1, 0x38);
        for fan in 0..FAN_COUNT {
            // Non-zero tach so `quiet` visits every fan.
            mock.seed(PAGE_ACPI, FAN_SPEED_BASE + fan * 2, 0x10);
            mock.seed(PAGE_FAN, CURRENT_PWM_BASE + fan, 50);
            for i in 0..8 {
                mock.seed(PAGE_FAN, CURVE_BASE + fan * CURVE_LEN + i, 30 + i as u8);
                mock.seed(PAGE_FAN, CURVE_BASE + fan * CURVE_LEN + 8 + i, 50);
            }
        }
        mock
    }

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "evga-fan-test-{}-{}-{}",
                std::process::id(),
                tag,
                NEXT.fetch_add(1, Ordering::SeqCst)
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("create temp dir");
            TempDir { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn quiet_cli() -> Cli {
        Cli {
            action: Action::Quiet,
        }
    }

    #[test]
    fn quiet_cli_plans_without_hardware() {
        let dir = TempDir::new("quiet-plan");
        assert_eq!(plan_action(&quiet_cli(), dir.path()).unwrap(), Plan::Quiet);
    }

    #[test]
    fn read_all_matches_python_state_shape() {
        let mut mock = seeded_mock();
        mock.seed(PAGE_ACPI, FAN_SPEED_BASE + 6, 0x34);
        mock.seed(PAGE_ACPI, FAN_SPEED_BASE + 7, 0x12);
        mock.seed(PAGE_FAN, ADVANCED_MODE, 0b0100_0000);
        let mut ec = Ec::new(mock);
        let state = read_all(&mut ec).expect("read_all");

        assert_eq!(state.acpi.len(), PAGE_LEN);
        assert_eq!(state.smartfan.len(), PAGE_LEN);
        assert_eq!(state.rpm.len(), FAN_COUNT);
        assert_eq!(state.curves.len(), FAN_COUNT);
        assert_eq!(state.rpm[0], 0x10);
        assert_eq!(state.rpm[3], 0x1234);
        assert_eq!(state.advanced, 0b0100_0000);
        assert_eq!(state.pwm_now[2], 50);
        assert_eq!(state.curves[0].len(), CURVE_LEN);

        // Serialise exactly like the Python tool's JSON: same keys and values.
        let json = serde_json::to_string(&state).expect("serialize");
        assert!(json.starts_with("{\"acpi\":["));
        for key in [
            "\"smartfan\":",
            "\"rpm\":",
            "\"pwm_now\":",
            "\"advanced\":",
            "\"curves\":",
        ] {
            assert!(json.contains(key), "missing {key} in {json}");
        }
        let round: EcState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round, state);
    }

    #[test]
    fn python_backup_with_spaces_still_parses() {
        // json.dumps default separators, plus fields the Rust tool also writes.
        let raw = r#"{"acpi": [1, 2, 3], "smartfan": [4, 5], "rpm": [0, 1000], "pwm_now": [10], "advanced": 3, "curves": [[1, 2]]}"#;
        let state: EcState = serde_json::from_str(raw).expect("python json parses");
        assert_eq!(state.acpi, vec![1, 2, 3]);
        assert_eq!(state.rpm, vec![0, 1000]);
        assert_eq!(state.advanced, 3);
    }

    #[test]
    fn backup_writes_json_and_restore_round_trips_bytes() {
        let dir = TempDir::new("backup");
        let mut mock = seeded_mock();
        mock.seed(PAGE_ACPI, 200, 0xAB);
        mock.seed(PAGE_FAN, 220, 0xCD);
        let mut ec = Ec::new(mock);

        let state = read_all(&mut ec).expect("read_all");
        let path = write_backup(dir.path(), "backup", &state).expect("backup");
        assert!(path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("backup-"));

        let loaded = load_state(&path).expect("load");
        assert_eq!(loaded, state);

        // Wipe the pages, then restore from disk.
        let mut ec = Ec::new(MockEc::default());
        restore_pages(&mut ec, &loaded).expect("restore");
        let after = read_all(&mut ec).expect("read_all after restore");
        assert_eq!(after.acpi, state.acpi);
        assert_eq!(after.smartfan, state.smartfan);
    }

    #[test]
    fn latest_backup_orders_by_stamp_then_suffix_not_mtime() {
        let dir = TempDir::new("latest");
        fs::write(dir.path().join("pre-quiet-20260101-000000.json"), b"{}").unwrap();
        let first = dir.path().join("backup-20260101-000000.json");
        fs::write(&first, b"{}").unwrap();
        let second = dir.path().join("backup-20260101-000000-1.json");
        fs::write(&second, b"{}").unwrap();
        let newest = dir.path().join("backup-20260102-000000.json");
        fs::write(&newest, b"{}").unwrap();
        // Touching an older backup must not make it win.
        fs::write(&first, b"{}").unwrap();
        assert_eq!(latest_backup(dir.path()).unwrap(), newest);

        // Same stamp: the numeric collision suffix decides, not the name order.
        fs::remove_file(&newest).unwrap();
        assert_eq!(latest_backup(dir.path()).unwrap(), second);

        // A hand-named backup must not outrank a timestamped one.
        fs::write(dir.path().join("backup-before-bios.json"), b"{}").unwrap();
        assert_eq!(latest_backup(dir.path()).unwrap(), second);

        let empty = TempDir::new("latest-empty");
        assert!(latest_backup(empty.path()).is_err());
    }

    #[test]
    fn restore_rejects_a_foreign_backup_before_opening_hardware() {
        let dir = TempDir::new("restore-foreign");
        let mut foreign = EcState {
            acpi: vec![0u8; PAGE_LEN],
            smartfan: vec![0u8; PAGE_LEN],
            rpm: Vec::new(),
            pwm_now: Vec::new(),
            advanced: 0,
            curves: Vec::new(),
        };
        foreign.acpi[0] = 0x34;
        foreign.acpi[1] = 0x12;
        let path = dir.path().join("backup-foreign.json");
        fs::write(&path, serde_json::to_vec(&foreign).unwrap()).unwrap();

        let opened = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&opened);
        let cli = Cli {
            action: Action::Restore { file: Some(path) },
        };
        let err = run_with::<MockEc, _>(cli, dir.path(), move || {
            flag.store(true, Ordering::SeqCst);
            Err(io::Error::other("must not open"))
        })
        .expect_err("a foreign backup must be rejected");
        assert!(err.to_string().contains("refusing to write"), "got {err}");
        assert!(!opened.load(Ordering::SeqCst), "hardware was opened anyway");
    }

    #[test]
    fn curve_command_backs_up_before_writing_and_writes_only_that_fan() {
        let dir = TempDir::new("curve");
        let cli = Cli {
            action: Action::Curve {
                fan: 3,
                vals: vec![
                    0, 40, 50, 60, 70, 127, 127, 127, 45, 55, 65, 80, 100, 100, 100, 100,
                ],
            },
        };
        let plan = plan_action(&cli, dir.path()).expect("plan");
        let mut mock = seeded_mock();
        mock.seed(PAGE_FAN, ADVANCED_MODE, 0);
        let mut ec = Ec::new(mock);
        execute(plan, &mut ec, dir.path()).expect("execute curve");

        let mock = ec.bus();
        let base = CURVE_BASE + 3 * CURVE_LEN;
        let temps: Vec<u8> = (0..8).map(|i| mock.page(PAGE_FAN)[base + i]).collect();
        assert_eq!(temps, vec![0, 40, 50, 60, 70, 127, 127, 127]);
        let pwms: Vec<u8> = (0..8).map(|i| mock.page(PAGE_FAN)[base + 8 + i]).collect();
        assert_eq!(pwms, vec![45, 55, 65, 80, 100, 100, 100, 100]);
        assert_eq!(mock.page(PAGE_FAN)[ADVANCED_MODE] & (1 << 3), 1 << 3);
        assert!(
            mock.writes_to(PAGE_FAN, CURVE_BASE + 4 * CURVE_LEN)
                .is_empty(),
            "fan 4 must not be touched"
        );

        // Exactly one backup, and it holds the state from before the write.
        let files: Vec<PathBuf> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(files.len(), 1, "one pre-curve backup, got {files:?}");
        assert!(files[0]
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("pre-curve-"));
        let saved: EcState = serde_json::from_slice(&fs::read(&files[0]).unwrap()).unwrap();
        assert_eq!(
            saved.advanced, 0,
            "backup must predate the advanced-bit write"
        );
        assert_ne!(saved.curves[3][0], 0, "backup holds the old fan 3 curve");
    }

    #[test]
    fn full_speed_must_be_reached_by_70c() {
        let bad = Curve {
            temps: [0, 40, 50, 60, 80, 127, 127, 127],
            pwms: [45, 55, 65, 80, 100, 100, 100, 100],
        };
        assert!(
            validate_curve(&bad, 45).is_err(),
            "100% at 80 C is too late even though a later point is 100%"
        );

        let at_60 = Curve {
            temps: [0, 40, 50, 60, 60, 127, 127, 127],
            pwms: [45, 55, 65, 100, 100, 100, 100, 100],
        };
        assert!(validate_curve(&at_60, 45).is_ok(), "100% at 60 C is fine");

        let at_70 = Curve {
            temps: [0, 40, 50, 60, 70, 127, 127, 127],
            pwms: [45, 55, 65, 80, 100, 100, 100, 100],
        };
        assert!(
            validate_curve(&at_70, 45).is_ok(),
            "100% at exactly 70 C is fine"
        );

        let never_full = Curve {
            temps: [0, 40, 50, 60, 70, 127, 127, 127],
            pwms: [45, 55, 65, 80, 99, 99, 99, 99],
        };
        assert!(
            validate_curve(&never_full, 45).is_err(),
            "a curve that never reaches 100% is unsafe"
        );
    }

    #[test]
    fn quiet_floor_is_at_least_45_and_full_speed_by_70c() {
        assert!(QUIET_PWMS[0] >= QUIET_MIN_PCT);
        assert_eq!(QUIET_PWMS[4], 100);
        let curve = Curve::from_slices(&QUIET_TEMPS, &QUIET_PWMS);
        validate_curve(&curve, QUIET_MIN_PCT).expect("quiet curve is valid");
        assert!(QUIET_TEMPS[4] <= FULL_SPEED_TEMP_C);
        assert!(QUIET_PWMS.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn quiet_writes_curves_and_advanced_bits_with_backup() {
        let dir = TempDir::new("quiet");
        let mut mock = seeded_mock();
        mock.seed(PAGE_FAN, ADVANCED_MODE, 0);
        let mut ec = Ec::new(mock);

        quiet_with(
            &mut ec,
            dir.path(),
            Timing {
                quiet_settle: Duration::ZERO,
                sweep_step: Duration::ZERO,
                sweep_settle: Duration::ZERO,
            },
        )
        .expect("quiet");

        let mock = ec.bus();
        for fan in 0..FAN_COUNT {
            let base = CURVE_BASE + fan * CURVE_LEN;
            let pwms: Vec<u8> = (0..8).map(|i| mock.page(PAGE_FAN)[base + 8 + i]).collect();
            assert_eq!(pwms, QUIET_PWMS.to_vec(), "fan {fan} duties");
            assert_eq!(mock.page(PAGE_FAN)[ADVANCED_MODE] & (1 << fan), 1 << fan);
        }
        assert!(fs::read_dir(dir.path()).unwrap().count() == 1);
    }

    #[test]
    fn quiet_skips_fans_without_a_tach() {
        let dir = TempDir::new("quiet-skip");
        let mut mock = seeded_mock();
        mock.seed(PAGE_ACPI, FAN_SPEED_BASE + 2 * 2, 0);
        mock.seed(PAGE_ACPI, FAN_SPEED_BASE + 2 * 2 + 1, 0);
        let mut ec = Ec::new(mock);
        quiet_with(
            &mut ec,
            dir.path(),
            Timing {
                quiet_settle: Duration::ZERO,
                sweep_step: Duration::ZERO,
                sweep_settle: Duration::ZERO,
            },
        )
        .expect("quiet");
        let mock = ec.bus();
        let base = CURVE_BASE + 2 * CURVE_LEN;
        assert!(
            mock.writes_to(PAGE_FAN, base).is_empty(),
            "fan 2 has no tach and must not be written"
        );
    }

    #[test]
    fn out_of_range_fan_is_rejected_before_hardware_open() {
        let dir = TempDir::new("validate-fan");
        let opened = Arc::new(AtomicBool::new(false));
        for fan in [-1, 8, 99] {
            let flag = Arc::clone(&opened);
            let cli = Cli {
                action: Action::Curve {
                    fan,
                    vals: vec![
                        0, 40, 50, 60, 70, 127, 127, 127, 45, 55, 65, 80, 100, 100, 100, 100,
                    ],
                },
            };
            let err = run_with::<MockEc, _>(cli, dir.path(), move || {
                flag.store(true, Ordering::SeqCst);
                Err(io::Error::other("must not open"))
            })
            .expect_err("fan index must be rejected");
            assert!(err.to_string().contains("out of range"), "got {err}");
        }
        assert!(!opened.load(Ordering::SeqCst), "hardware was opened anyway");
    }

    #[test]
    fn unsafe_curves_are_rejected_before_hardware_open() {
        let dir = TempDir::new("validate-curve");
        let open_calls = Arc::new(AtomicUsize::new(0));
        let cases: Vec<(Vec<i64>, &str)> = vec![
            (
                vec![
                    0, 40, 50, 60, 70, 127, 127, 127, 30, 55, 65, 80, 100, 100, 100, 100,
                ],
                "below",
            ),
            (
                vec![
                    0, 40, 50, 60, 70, 127, 127, 127, 45, 55, 65, 80, 99, 100, 100, 100,
                ],
                "100%",
            ),
            (
                vec![
                    0, 40, 50, 60, 70, 127, 127, 127, 45, 55, 65, 80, 101, 100, 100, 100,
                ],
                "0..=100",
            ),
            (
                vec![
                    0, 40, 50, 60, 55, 127, 127, 127, 45, 55, 65, 80, 100, 100, 100, 100,
                ],
                "temperatures must not decrease",
            ),
            (
                vec![
                    0, 40, 50, 60, 70, 127, 127, 127, 45, 55, 65, 80, 100, 95, 100, 100,
                ],
                "duties must not decrease",
            ),
            (
                vec![
                    0, 40, 50, 60, 70, 127, 127, 128, 45, 55, 65, 80, 100, 100, 100, 100,
                ],
                "0..=127",
            ),
        ];
        for (vals, needle) in cases {
            let calls = Arc::clone(&open_calls);
            let cli = Cli {
                action: Action::Curve { fan: 1, vals },
            };
            let err = run_with::<MockEc, _>(cli, dir.path(), move || {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(io::Error::other("must not open"))
            })
            .expect_err("curve must be rejected");
            assert!(
                err.to_string().contains(needle),
                "expected {needle:?} in error, got {err}"
            );
        }
        assert_eq!(open_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn write_curve_writes_16_bytes_and_advanced_bit() {
        let mut mock = seeded_mock();
        mock.seed(PAGE_FAN, ADVANCED_MODE, 0b0000_0001);
        let mut ec = Ec::new(mock);
        let curve = Curve::from_slices(&QUIET_TEMPS, &QUIET_PWMS);
        write_curve(&mut ec, 5, &curve, true).expect("write");
        write_curve(&mut ec, 0, &curve, false).expect("clear bit");

        let mock = ec.bus();
        let base = CURVE_BASE + 5 * CURVE_LEN;
        assert_eq!(mock.page(PAGE_FAN)[base], QUIET_TEMPS[0]);
        assert_eq!(mock.page(PAGE_FAN)[base + 8], QUIET_PWMS[0]);
        assert_eq!(mock.page(PAGE_FAN)[ADVANCED_MODE], 0b0010_0000);
    }

    #[test]
    fn sweep_restores_the_exact_original_curve_after_a_fault() {
        let dir = TempDir::new("sweep-fault");
        let mut mock = seeded_mock();
        // The mock only counts page-data writes, and the backup performs none,
        // so this fault lands inside the first probe curve, after the backup.
        mock.fail_once_at = Some(20);
        let mut ec = Ec::new(mock);

        let err = sweep_with(
            &mut ec,
            7,
            dir.path(),
            Timing {
                quiet_settle: Duration::ZERO,
                sweep_step: Duration::ZERO,
                sweep_settle: Duration::ZERO,
            },
            false,
        )
        .expect_err("injected fault must fail the sweep");
        assert!(
            err.to_string().contains("injected EC write fault"),
            "got {err}"
        );

        // The fan is back to its seeded 16 curve bytes and its cleared
        // AdvancedMode bit, i.e. byte for byte what the sweep found.
        let mock = ec.bus();
        let base = CURVE_BASE + 7 * CURVE_LEN;
        let temps: Vec<u8> = (0..8).map(|i| mock.page(PAGE_FAN)[base + i]).collect();
        let expected_temps: Vec<u8> = (0..8).map(|i| 30 + i as u8).collect();
        assert_eq!(temps, expected_temps, "temperatures restored");
        let pwms: Vec<u8> = (0..8).map(|i| mock.page(PAGE_FAN)[base + 8 + i]).collect();
        assert_eq!(pwms, vec![50u8; 8], "duties restored");
        assert_eq!(
            mock.page(PAGE_FAN)[ADVANCED_MODE] & (1 << 7),
            0,
            "AdvancedMode bit restored to its original 0"
        );
        // Fan 0 must be untouched by a sweep of fan 7.
        assert!(mock.writes_to(PAGE_FAN, CURVE_BASE).is_empty());

        let backups: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(backups.len(), 1, "one pre-sweep backup, got {backups:?}");
        assert!(backups[0].starts_with("pre-sweep-"));
    }

    #[test]
    fn sweep_settles_at_45_or_above_after_finding_the_floor() {
        let dir = TempDir::new("sweep-ok");
        let mut mock = seeded_mock();
        // Fan 7 spins from 35%; the sweep must find that and settle at 45%.
        mock.tach_floor = Some((7, 35));
        let mut ec = Ec::new(mock);

        sweep_with(
            &mut ec,
            7,
            dir.path(),
            Timing {
                quiet_settle: Duration::ZERO,
                sweep_step: Duration::ZERO,
                sweep_settle: Duration::ZERO,
            },
            false,
        )
        .expect("sweep");

        let mock = ec.bus();
        let base = CURVE_BASE + 7 * CURVE_LEN;
        let pwms: Vec<u8> = (0..8).map(|i| mock.page(PAGE_FAN)[base + 8 + i]).collect();
        assert_eq!(pwms, vec![45, 55, 70, 85, 100, 100, 100, 100]);
        assert_eq!(
            mock.page(PAGE_FAN)[base + 4],
            FULL_SPEED_TEMP_C,
            "100% at 70 C"
        );
        assert_eq!(mock.page(PAGE_FAN)[ADVANCED_MODE] & (1 << 7), 1 << 7);
        // The settled curve spins the fan in the model.
        let state = read_all(&mut Ec::new((*mock).clone())).expect("read back");
        assert_eq!(state.rpm[7], 1200);
    }

    #[test]
    fn sweep_uses_duty_40_floor_as_a_50_start() {
        let dir = TempDir::new("sweep-floor40");
        let mut mock = seeded_mock();
        mock.tach_floor = Some((7, 40));
        let mut ec = Ec::new(mock);
        sweep_with(
            &mut ec,
            7,
            dir.path(),
            Timing {
                quiet_settle: Duration::ZERO,
                sweep_step: Duration::ZERO,
                sweep_settle: Duration::ZERO,
            },
            false,
        )
        .expect("sweep");
        let mock = ec.bus();
        let base = CURVE_BASE + 7 * CURVE_LEN;
        let pwms: Vec<u8> = (0..8).map(|i| mock.page(PAGE_FAN)[base + 8 + i]).collect();
        assert_eq!(pwms, vec![50, 60, 75, 85, 100, 100, 100, 100]);
    }

    #[test]
    fn sweep_margins_off_the_measured_duty_not_the_requested_point() {
        let dir = TempDir::new("sweep-measured");
        let mut mock = seeded_mock();
        // The fan spins from a requested 40%, but the EC reports it is actually
        // driving 50% at that sensor temperature.
        mock.tach_floor = Some((7, 40));
        mock.pwm_bias = 10;
        let mut ec = Ec::new(mock);
        sweep_with(
            &mut ec,
            7,
            dir.path(),
            Timing {
                quiet_settle: Duration::ZERO,
                sweep_step: Duration::ZERO,
                sweep_settle: Duration::ZERO,
            },
            false,
        )
        .expect("sweep");
        let mock = ec.bus();
        let base = CURVE_BASE + 7 * CURVE_LEN;
        let pwms: Vec<u8> = (0..8).map(|i| mock.page(PAGE_FAN)[base + 8 + i]).collect();
        assert_eq!(
            pwms,
            vec![60, 70, 85, 85, 100, 100, 100, 100],
            "base = measured 50 + 10 margin"
        );
    }

    #[test]
    fn sweep_keeps_the_margin_when_the_measured_duty_is_high() {
        let dir = TempDir::new("sweep-hot");
        let mut mock = seeded_mock();
        // Spins at a requested 40%, but the EC reports 80% applied.
        mock.tach_floor = Some((7, 40));
        mock.pwm_bias = 40;
        let mut ec = Ec::new(mock);
        sweep_with(
            &mut ec,
            7,
            dir.path(),
            Timing {
                quiet_settle: Duration::ZERO,
                sweep_step: Duration::ZERO,
                sweep_settle: Duration::ZERO,
            },
            false,
        )
        .expect("sweep");
        let mock = ec.bus();
        let base = CURVE_BASE + 7 * CURVE_LEN;
        let pwms: Vec<u8> = (0..8).map(|i| mock.page(PAGE_FAN)[base + 8 + i]).collect();
        assert_eq!(
            pwms,
            vec![90, 100, 100, 100, 100, 100, 100, 100],
            "base = measured 80 + 10, later points clamped to 100"
        );
        assert!(
            pwms.windows(2).all(|w| w[0] <= w[1]),
            "settled duties must stay non-decreasing"
        );
    }

    #[test]
    fn sweep_refuses_an_unverified_pwm_unit_and_restores() {
        let dir = TempDir::new("sweep-unit");
        let mut mock = seeded_mock();
        // A board that reports Current_PWM as raw 0..255.
        mock.tach_floor = Some((7, 20));
        mock.pwm_bias = 200;
        let mut ec = Ec::new(mock);
        let err = sweep_with(
            &mut ec,
            7,
            dir.path(),
            Timing {
                quiet_settle: Duration::ZERO,
                sweep_step: Duration::ZERO,
                sweep_settle: Duration::ZERO,
            },
            false,
        )
        .expect_err("an out-of-range Current_PWM must stop the sweep");
        assert!(err.to_string().contains("outside 0..=100"), "got {err}");

        let mock = ec.bus();
        let base = CURVE_BASE + 7 * CURVE_LEN;
        let temps: Vec<u8> = (0..8).map(|i| mock.page(PAGE_FAN)[base + i]).collect();
        assert_eq!(temps, (0..8).map(|i| 30 + i as u8).collect::<Vec<_>>());
        let pwms: Vec<u8> = (0..8).map(|i| mock.page(PAGE_FAN)[base + 8 + i]).collect();
        assert_eq!(pwms, vec![50u8; 8], "original duties restored");
        assert_eq!(mock.page(PAGE_FAN)[ADVANCED_MODE] & (1 << 7), 0);
    }

    #[test]
    fn sweep_without_a_spinning_floor_fails_and_restores() {
        let dir = TempDir::new("sweep-nospin");
        // No tach model: the fan reads its seeded 16 RPM, below MIN_SPIN_RPM.
        let mut ec = Ec::new(seeded_mock());

        let err = sweep_with(
            &mut ec,
            5,
            dir.path(),
            Timing {
                quiet_settle: Duration::ZERO,
                sweep_step: Duration::ZERO,
                sweep_settle: Duration::ZERO,
            },
            false,
        )
        .expect_err("a fan that never spins must not get a settled curve");
        assert!(
            err.to_string().contains("no duty in"),
            "expected a no-floor error, got {err}"
        );

        let mock = ec.bus();
        let base = CURVE_BASE + 5 * CURVE_LEN;
        let temps: Vec<u8> = (0..8).map(|i| mock.page(PAGE_FAN)[base + i]).collect();
        assert_eq!(temps, (0..8).map(|i| 30 + i as u8).collect::<Vec<_>>());
        let pwms: Vec<u8> = (0..8).map(|i| mock.page(PAGE_FAN)[base + 8 + i]).collect();
        assert_eq!(pwms, vec![50u8; 8], "original duties restored");
        assert_eq!(
            mock.page(PAGE_FAN)[ADVANCED_MODE] & (1 << 5),
            0,
            "AdvancedMode bit restored"
        );
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn sweep_reports_a_failed_restore_separately() {
        let dir = TempDir::new("sweep-both");
        let mut mock = seeded_mock();
        mock.fail_all = true;
        let mut ec = Ec::new(mock);

        let err = sweep_with(
            &mut ec,
            1,
            dir.path(),
            Timing {
                quiet_settle: Duration::ZERO,
                sweep_step: Duration::ZERO,
                sweep_settle: Duration::ZERO,
            },
            false,
        )
        .expect_err("a failing EC must fail the sweep");
        assert!(
            err.to_string().contains("injected EC write fault"),
            "got {err}"
        );
        // No data write landed, so the curve is untouched, and the pre-sweep
        // backup is the fallback the user is told about.
        let mock = ec.bus();
        let base = CURVE_BASE + CURVE_LEN;
        assert!(mock.writes_to(PAGE_FAN, base).is_empty());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn sweep_rejects_an_out_of_range_fan_before_opening_hardware() {
        let dir = TempDir::new("sweep-validate");
        let opened = Arc::new(AtomicBool::new(false));
        for fan in [-1, 8, 200] {
            let flag = Arc::clone(&opened);
            let err = sweep_run_with::<MockEc, _>(fan, dir.path(), move || {
                flag.store(true, Ordering::SeqCst);
                Err(io::Error::other("must not open"))
            })
            .expect_err("fan index must be rejected");
            assert!(err.to_string().contains("out of range"), "got {err}");
        }
        assert!(!opened.load(Ordering::SeqCst), "hardware was opened anyway");
    }

    #[test]
    fn restore_rejects_incomplete_pages_before_opening_hardware() {
        let dir = TempDir::new("restore-short");
        let path = dir.path().join("backup-short.json");
        fs::write(&path, br#"{"acpi": [1, 2, 3], "smartfan": [4, 5, 6]}"#).unwrap();
        let opened = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&opened);
        let cli = Cli {
            action: Action::Restore {
                file: Some(path.clone()),
            },
        };
        let err = run_with::<MockEc, _>(cli, dir.path(), move || {
            flag.store(true, Ordering::SeqCst);
            Err(io::Error::other("must not open"))
        })
        .expect_err("short pages must be rejected");
        assert!(err.to_string().contains("exactly 256"), "got {err}");
        assert!(!opened.load(Ordering::SeqCst), "hardware was opened anyway");
    }

    #[test]
    fn restore_backs_up_the_current_pages_before_overwriting() {
        let dir = TempDir::new("restore-backup");
        let mut mock = seeded_mock();
        mock.seed(PAGE_ACPI, 200, 0xAB);
        let mut ec = Ec::new(mock);
        let current = read_all(&mut ec).expect("read current");

        // A target state that differs from the current pages.
        let mut target = current.clone();
        target.acpi[200] = 0x11;
        target.smartfan[220] = 0x22;
        let target_path = dir.path().join("backup-target.json");
        fs::write(&target_path, serde_json::to_vec(&target).unwrap()).unwrap();

        let cli = Cli {
            action: Action::Restore {
                file: Some(target_path),
            },
        };
        let plan = plan_action(&cli, dir.path()).expect("plan");
        execute(plan, &mut ec, dir.path()).expect("restore");

        let after = read_all(&mut ec).expect("read after");
        assert_eq!(after.acpi[200], 0x11);
        assert_eq!(after.smartfan[220], 0x22);

        let pre: Vec<PathBuf> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("pre-restore-")
            })
            .collect();
        assert_eq!(pre.len(), 1, "one pre-restore backup, got {pre:?}");
        let saved: EcState = serde_json::from_slice(&fs::read(&pre[0]).unwrap()).unwrap();
        assert_eq!(saved.acpi[200], 0xAB, "backup holds the pre-restore page");
    }
}
