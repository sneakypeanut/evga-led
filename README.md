# evga-led

Turn the onboard ARGB lighting of an **EVGA Z790 CLASSIFIED** on or off from Linux —
no Windows, no EVGA ELEET X1, no vendor driver.

```console
$ evga-led off          # lights out
$ evga-led on           # back to static white
$ evga-led mode rainbow # any effect the MCU knows
$ evga-led status       # what the controller last reported
```

Why this exists: EVGA's ELEET X1 is Windows-only, OpenRGB has no EVGA
*motherboard* driver (only EVGA GPUs), and the Z790 CLASSIFIED's BIOS has no LED
switch. This speaks the board's own HID protocol directly.

## Supported hardware

| Board | Status |
|---|---|
| EVGA Z790 CLASSIFIED (121-BL-E790), BIOS 1.20 | **tested working** |
| EVGA Z690 DARK / CLASSIFIED | expected — `EDispNetLib.dll` shares `LEDControlZ690` between them; unverified |
| Other EVGA boards | unverified; the protocol library ships per-board variants |

The LED controller is the on-board MCU exposed as USB **`3842:300e` "EVGA 5516 MCU AP"**.
A kernel driver is *not* needed; this talks to the HID feature report via
`/dev/hidrawN`.

## Install

```bash
install -m755 evga-led ~/.local/bin/evga-led

# let your user (not root) open the hidraw node:
sudo install -m644 99-evga-led.rules /etc/udev/rules.d/
sudo udevadm control --reload && sudo udevadm trigger
```

If you would rather not install a udev rule, `evga-led` falls back to running
itself inside a container with `--device` access (docker group = root-equivalent):

```bash
EVGA_LED_IMAGE=python:3-slim evga-led off
```

## Usage

```
evga-led [--device /dev/hidrawN] <action> [-z ZONE] [-n]

actions:  off | on | mode <name> | status | detect
zones:    all (default) | onboard | argb1 | argb2 | argb3 | argb4
modes:    off on breathing rainbow pulse rainbowwave wave star colorstack
```

`-n/--dry-run` prints the exact 226-byte frame without touching the device.
`onboard` is `ARGB_COVER` — the lit "Z790 | CLASSIFIED" text on the rear I/O
shroud.

## The protocol

Reverse engineered from ELEET X1 **1.0.16.0** (`Library/EDispNetLib.dll`,
namespace `EDispNetLib.Hardwares.Z790`: `Led`, `LEDUsbCommand`, `LEDModeReport`,
`LEDMainCommand`, `LEDSubCommand`, `LEDZone`).

One HID **feature report, report ID 7** (225-byte payload; 226-byte buffer
including the report ID byte). Windows uses `HidD_SetFeature` + `HidD_GetFeature`;
the Linux equivalents are `HIDIOCSFEATURE` / `HIDIOCGFEATURE` on the hidraw node.

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

Main commands: `1` LightEffect, `2` LEDNumber, `3` StaticOn, `4` Breathing,
`5` Rainbow, `6` Pulse, `7` RainbowWave, `8` Wave, `9` Star, `10` ColorStack,
`32` SpeedRange.

Modes: `0` StaticOff, `1` StaticOn, `2` Breathing, `3` Rainbow, `4` Pulse,
`5` RainbowWave, `6` Wave, `7` Star, `8` ColorStack.

Example — everything off (checksum `0xED`):

```
07 ea 02 01 00 00 00 ed 1f ...
```

## Firmware quirks (verified, and why the code looks defensive)

- A **plain GET** returns the MCU's last command frame and works fine.
- **After a SET, an immediate GET stalls with `EPIPE`.** The command *is* applied —
  read-back is advisory only, so don't treat the stall as failure.
- A SET on **report ID 0 of the other interface** (`hidraw5`) can wedge the LED
  interface until a USB reset. If that happens:
  `USBDEVFS_RESET` (`0x5514`) on `/dev/bus/usb/BBB/DDD`. State survives the reset.
- LED state survives reboots while standby power is present; a full AC disconnect
  may restore the MCU default.
- Report ID 7 is only present on one of the two HID interfaces — the tool picks it
  by scanning `report_descriptor` for `85 07`.

## Notes

- Unofficial and unaffiliated with EVGA. It writes an undocumented vendor
  protocol; in principle an MCU could be left in a state that needs a power
  cycle. Flash/firmware reports are **not** touched by this tool.
- No EVGA code or binaries are redistributed here; only the field layout and
  command values observed from the vendor utility.
- MIT licensed.

Contributions of captures from other EVGA boards are welcome — `EDispNetLib.dll`
ships `Z490/Z590/Z690/Z790/W999/X570` variants, so the same frame with different
zone/mode tables probably covers several of them.
