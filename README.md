# evga-z790-tools

A single Rust CLI for Linux control of the **EVGA Z790 CLASSIFIED**'s ARGB
lighting and fan controller. No Windows, no EVGA ELEET X1, no vendor driver.

```console
$ evga led off                     # lights out (USB HID, no root)
$ evga fan show                    # fan RPM, duty and curves (raw I/O ports, root)
$ evga fan quiet                   # gentler curves, 100% still at 70 C
```

Both controller protocols were reverse engineered from **ELEET X1 1.0.16.0**.
The single CLI uses two independent hardware transports:

| | `evga led` | `evga fan` |
|---|---|---|
| Device | `3842:300e` "EVGA 5516 MCU AP" | Super I/O EC, I/O ports `0x4E`/`0x4F` |
| Transport | HID feature reports (`/dev/hidrawN`) | raw port I/O (`/dev/port`) |
| Privileges | udev `uaccess` rule → normal user | **root** (no safe unprivileged path) |
| Controls | ARGB zones, effects, global dark mode | 8 fans: RPM, live PWM, 8-point curves |

Supported: **EVGA Z790 CLASSIFIED (121-RL-E798), BIOS 1.20**. The original
Python tools were tested on hardware. Rust LED detection and read-back have been
verified; Rust fan writes have not yet been tested on hardware. Other EVGA boards
are unverified; ELEET ships per-board variants
(`Z490/Z590/Z690/Z790/W999/X570`).

---

# evga led — ARGB lighting

The board's onboard ARGB is the illuminated "Z790 | CLASSIFIED" text on the rear
I/O shroud (`ARGB_COVER`), plus four addressable headers (`ARGB1`–`ARGB4`).

```
evga led [--device /dev/hidrawN] [action] [-z ZONE] [-n]

actions:  off | on | mode <name> | status | detect
zones:    all (default) | onboard | argb1 | argb2 | argb3 | argb4
modes:    off on breathing rainbow pulse rainbowwave wave star colorstack
```

With no action, `evga led` shows status.

`-n/--dry-run` prints the exact 226-byte frame without touching the device.

## Install

```bash
# Build the native binary (Rust stable on Linux):
cargo build --release --locked
install -Dm755 target/release/evga ~/.local/bin/evga

# let your user (not root) open the hidraw node:
pkexec install -m644 "$PWD/60-evga-led.rules" /etc/udev/rules.d/
pkexec udevadm control --reload
pkexec udevadm trigger --subsystem-match=hidraw
```

This replaces the earlier `evga-led`, `evga-fan`, `evga-fan-probe` and
`evga-fan-sweep` executables. Previously installed copies can be removed after
installing `evga`.

The rule **must** be numbered below `73-seat-late.rules` (hence `60-`): udev only
acts on the `uaccess` tag before that point, so a `99-` rule installs cleanly and
silently grants nothing.

Install the udev rule for direct LED access as your normal user. If permissions
are missing, the CLI reports the rule to install. No container runtime is needed.

## Protocol

`Library/EDispNetLib.dll`, namespace `EDispNetLib.Hardwares.Z790` (`Led`,
`LEDUsbCommand`, `LEDModeReport`, `LEDMainCommand`, `LEDSubCommand`, `LEDZone`).

One HID **feature report, report ID 7** (225-byte payload; 226-byte buffer
including the report ID). Windows uses `HidD_SetFeature` + `HidD_GetFeature`;
Linux uses `HIDIOCSFEATURE` / `HIDIOCGFEATURE` on the hidraw node.

```
[0]     0x07                 report id
[1..2]  0xEA 0x02            header
[3]     0x01                 main command: LightEffect
[4]     0x00 / 0x01 / 0x02   sub command: Write / Read / ReadDefault
[5]     0x00                 sub command 2: None
[6]     0xC0                 response: 0xC0 Success, 0xC1 Fail, 0xC2 InProcess
[7]     checksum             two's complement of sum(bytes 0..6, 8..13)
[8]     zone bitmask         ARGB_1=0x01 ARGB_2=0x02 ARGB_3=0x04 ARGB_4=0x08
                             ARGB_COVER/onboard=0x10, all=0x1F
[9..13] mode per zone        in LEDZones order: ARGB_1..4, ARGB_COVER
[14..225] reserved zeros
```

Commands: `1` LightEffect, `2` LEDNumber, `3` StaticOn, `4` Breathing, `5` Rainbow,
`6` Pulse, `7` RainbowWave, `8` Wave, `9` Star, `10` ColorStack, `32` SpeedRange.
Modes: `0` StaticOff, `1` StaticOn, `2` Breathing, `3` Rainbow, `4` Pulse,
`5` RainbowWave, `6` Wave, `7` Star, `8` ColorStack.

Everything off (checksum `0xED`): `07 ea 02 01 00 00 00 ed 1f ...`

The same MCU also has a **global dark mode** on report ID 4
(`Data1Report`: 7-byte command + data + 9 reserved), `GeneralMainCommand.LEDDarkMode = 80`,
data `0` = dark, `1` = lit. Not yet exposed as a CLI flag.

## Firmware quirks

- A **GET** returns the MCU's last command frame, including right after a SET.
  The buffer's first byte must be the report ID (`7`): with `0`, the interface
  (which declares only IDs 3–8) stalls the request with `EPIPE`.
- A SET on **report ID 0 of the other interface** (`hidraw5`) can wedge the LED
  interface until a `USBDEVFS_RESET` (`0x5514`) on `/dev/bus/usb/BBB/DDD`.
- **LED state survives a USB reset.** After a `USBDEVFS_RESET` the onboard
  "Z790 | CLASSIFIED" text stayed off and the MCU still reported the last command
  for all zones (BIOS 1.20, Nov 2026; ARGB1–4 headers had nothing attached, so
  only their reported state was checked). Reset only when wedged.
- Report ID 7 lives on only one of the two HID interfaces — the tool picks it by
  scanning `report_descriptor` for `85 07`.

---

# evga fan — fan curves

```
evga fan backup            dump both EC pages to ~/.local/state/evga-fan/
evga fan show              RPM, live duty, curves per fan
evga fan quiet             write gentle curves (never below the stall floor)
evga fan curve FAN t0..t7 p0..p7
evga fan restore [FILE]    write both pages back verbatim
evga fan probe             detailed EC identity and fan state
evga fan sweep [FAN]       find a fan's stall floor (default: fan 7)
```

Fans 0–7 correspond to the board's fan headers; headers with nothing plugged in
read `0 RPM` and are skipped by `quiet`.

The single `evga` executable replaces the Python scripts. LED actions and frame
bytes remain compatible under `evga led`; fan backups retain the `acpi` and `smartfan` JSON arrays so
existing backups can be restored. The Rust fan commands validate input before
accessing the EC and save a backup before changing its configuration.

`quiet` now starts at 45% duty rather than the Python preset's 25%, keeping ten
percentage points above the observed 35% stall threshold. A different fan may
need a higher floor. Custom curves must use nondecreasing temperatures and duties,
with full duty by 70 °C and a minimum of 35%. Restore is a verbatim recovery
operation and can reapply curves below these limits.

`evga fan probe` prints EC identity and detailed fan state. `evga fan sweep [FAN]`
measures a fan's low-duty behavior and settles a curve above the measured floor;
it deliberately tries duties as low as 20% and should be run only while watching
temperatures. An I/O failure triggers an attempt to restore the original curve;
forced termination or power loss can leave the last curve active. The pre-sweep
backup remains available for recovery.

Sweep treats the EC's `Current_PWM` reading as a percentage, following the
original tool. Its scale has not been verified on hardware; a raw reading above
100 aborts the sweep and triggers an attempt to restore the original curve.
Check the probe's readings before using sweep to tune a fan.

## Root is required

`evga fan` writes I/O ports `0x4E`/`0x4F` through `/dev/port`, which needs
`CAP_SYS_RAWIO`. There is deliberately no udev rule for this: a `kmem` ACL for a
normal user is a much larger hole than a hidraw node. Run the native binary with `pkexec`:

```bash
pkexec "$HOME/.local/bin/evga" fan show
```

The state directory follows the process's `HOME` or `EVGA_FAN_STATE`. `pkexec`
uses the target user's environment, so the default for a root invocation is
normally `/root/.local/state/evga-fan/`. To choose an explicit backup directory:

```bash
pkexec env EVGA_FAN_STATE="$HOME/.local/state/evga-fan" \
  "$HOME/.local/bin/evga" fan backup
```

Use the same directory for later commands, or give `restore` an absolute backup
path. Files created through `pkexec` are owned by root, including those written
to a directory under your home. Restore rejects backups without full EC pages
and an EVGA vendor ID; it still restores the page bytes verbatim, including any
manual fan settings.

## Protocol

`Library/SmartFan.dll` (`SmartFan.Hardware.EC`, `MySmartFan`,
`SmartFan.Protocols.{AcpiRamProtocol,SmartFanProtocol,CurvePointDefine,FanControlPWMDefine}`).

EC access (ITE-style, index register `0x2E`, data register `0x2F`, both behind
ports `0x4E`/`0x4F`):

```
write 0x2E -> 0x4E ; write <reg> -> 0x4F     select an EC register
write 0x2F -> 0x4E                           select its data byte
read/write 0x4F
```

`ECReg`: `Reg=0x10 Page=0x11 Data=0x12`. `ECPage`: `AcpiRam=0xC4 SmartFan=0xC7 SmartFanRom=0xD1`.

Page `0xC4` (`AcpiRamProtocol`):

| offset | content |
|---|---|
| 0 / 2 | VendorID (u16, `0x3842`) / DeviceID (u16) |
| 144 | `FanControlPWMs[8]`, 2 bytes each: `FanControl`, `PWM` |
| 160 | `FanSpeeds[8]`, u16 LE, RPM |
| 176 | `FanMode`, u16, 2 bits per fan |

`FanControl` byte: bit 7 = manual, bits 4–6 = sensor source, bits 0–3 = sensor index.

Page `0xC7` (`SmartFanProtocol`):

| offset | content |
|---|---|
| 0 / 8 | `FanLevel[8]` / `FanTolerance[8]` |
| 16 | curves: 8 fans × (`8 temps` + `8 pwms`) = 128 bytes |
| 144 / 152 | `Fan_Step_Modes[8]` / `Current_PWM[8]` |
| 216 | `AdvancedMode`, one bit per fan: 1 = use the custom curve |

## Safety rules (learned the hard way)

- **Back up before writing.** `evga fan backup` dumps both pages; `restore` puts
  them back. Do this before any curve edit.
- **Never set a duty below a fan's stall floor.** Measured on this board: one fan
  stops entirely at 20–30% and only spins from 35% up. A stalled fan reads `0 RPM`
  and silently provides no cooling. Sweep with `evga fan sweep` to find the floor per
  fan, then sit ≥10 points above it.
- **Always keep 100% by ~70 °C.** Soften only the low end of the curve; leave the
  top end intact so load behaviour is unchanged.
- Prefer editing **curves** over forcing manual PWM: the EC keeps ramping on its
  own, so nothing breaks if the process dies or the machine reboots.
- Whether the BIOS rewrites the curves at boot is **unverified** — check after a
  reboot, and use a boot-time service if it does.

---

## Repository layout

```
src/led.rs              ARGB protocol, HID transport, CLI
src/fan.rs              EC transport, fan control, probe and sweep
src/main.rs             single evga CLI and main function
tests/                  CLI checks and Python-derived LED parity fixtures
60-evga-led.rules       udev rule: hidraw uaccess for 3842:300e
```

## Notes

Development checks (no hardware required):

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
```

CLI tests compare all 77 mode/zone alias combinations against dry-run output
captured from the original Python implementation. Unit tests cover the hardware
protocols with simulated transports. Passing these checks does not establish
that the Rust binaries have been tested on the physical board.

- Unofficial and unaffiliated with EVGA. These tools write undocumented vendor
  protocols. The fan path touches live cooling configuration — keep a backup and
  watch temperatures after a change. Flash/firmware commands are not used.
- No EVGA code or binaries are redistributed here; only field layouts and command
  values observed from the vendor utility.
- MIT licensed.
