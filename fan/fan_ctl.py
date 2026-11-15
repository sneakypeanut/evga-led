#!/usr/bin/env python3
"""EVGA Z790 fan control via the Super I/O EC (reverse engineered from ELEET X1).

    fan_ctl.py backup            dump both EC pages to ~/.local/state/evga-fan/
    fan_ctl.py show              current RPM / PWM / curves
    fan_ctl.py quiet             write gentle curves (keeps 100% by 70 C)
    fan_ctl.py curve FAN T... P.. write one fan's 8 temps + 8 pwms
    fan_ctl.py restore FILE      write both pages back verbatim

EC access: ports 0x4E/0x4F, index reg 0x2E, data reg 0x2F.
  ECReg  Reg=0x10 Page=0x11 Data=0x12
  ECPage AcpiRam=0xC4  SmartFan=0xC7
  0xC4: FanControlPWMs@144 (2 B/fan: control,PWM)  FanSpeeds@160 (u16 RPM)
        FanMode@176 (u16, 2 bits/fan)
  0xC7: FanLevel@0(8) FanTolerance@8(8) Fans@16 (8 x [8 temps][8 pwms])
        Fan_Step_Modes@144(8) Current_PWM@152(8) Reserved@160(56) AdvancedMode@216
FanControl byte: bit7 manual, bits 4-6 sensor source, bits 0-3 sensor index.
AdvancedMode: one bit per fan; 1 = use the custom curve above.
"""
import argparse
import json
import os
import sys
import time
from pathlib import Path

PORT = "/dev/port"
INDEX_REG, DATA_REG = 0x2E, 0x2F
PORT_INDEX, PORT_DATA = 0x4E, 0x4F
EREG_REG, EREG_PAGE, EREG_DATA = 0x10, 0x11, 0x12
ACPI, FAN = 0xC4, 0xC7
STATE = Path(os.environ.get("EVGA_FAN_STATE")
             or Path.home() / ".local/state/evga-fan")

FAN_CONTROL_BASE = 144          # 0xC4: 2 bytes per fan
FAN_SPEED_BASE = 160            # 0xC4: u16 LE per fan
CURVE_BASE = 16                 # 0xC7: 16 bytes per fan
CURVE_LEN = 16
ADVANCED_MODE = 216


class EC:
    def __init__(self):
        self.fd = os.open(PORT, os.O_RDWR)

    def _wr(self, port, val):
        os.pwrite(self.fd, bytes([val & 0xFF]), port)

    def _rd(self, port):
        return os.pread(self.fd, 1, port)[0]

    def _reg(self, ecreg, value=None):
        self._wr(PORT_INDEX, INDEX_REG)
        self._wr(PORT_DATA, ecreg)
        self._wr(PORT_INDEX, DATA_REG)
        if value is None:
            return self._rd(PORT_DATA)
        self._wr(PORT_DATA, value)

    def read(self, page, reg):
        self._reg(EREG_PAGE, page)
        self._reg(EREG_REG, reg)
        return self._reg(EREG_DATA)

    def write(self, page, reg, value):
        self._reg(EREG_PAGE, page)
        self._reg(EREG_REG, reg)
        self._reg(EREG_DATA, value)

    def dump(self, page, n=256):
        return [self.read(page, r) for r in range(n)]

    def close(self):
        os.close(self.fd)


def read_all(ec):
    acpi = ec.dump(ACPI)
    fan = ec.dump(FAN)
    return {
        "acpi": acpi,
        "smartfan": fan,
        "rpm": [acpi[FAN_SPEED_BASE + f * 2] | (acpi[FAN_SPEED_BASE + f * 2 + 1] << 8)
                for f in range(8)],
        "pwm_now": [fan[152 + f] for f in range(8)],
        "advanced": fan[ADVANCED_MODE],
        "curves": [fan[CURVE_BASE + f * CURVE_LEN: CURVE_BASE + (f + 1) * CURVE_LEN]
                   for f in range(8)],
    }


def show(ec):
    s = read_all(ec)
    print(f"{'fan':>3} {'RPM':>6} {'PWM%':>5} {'adv':>4}  temps -> pwms")
    for f in range(8):
        c = s["curves"][f]
        adv = (s["advanced"] >> f) & 1
        print(f"{f:>3} {s['rpm'][f]:>6} {s['pwm_now'][f]:>5} {adv:>4}  "
              f"{c[:6]} -> {c[8:14]}")
    return s


def write_curve(ec, fan, temps, pwms):
    base = CURVE_BASE + fan * CURVE_LEN
    for i in range(8):
        ec.write(FAN, base + i, temps[i])
    for i in range(8):
        ec.write(FAN, base + 8 + i, pwms[i])
    # enable advanced (custom curve) mode for this fan
    adv = ec.read(FAN, ADVANCED_MODE)
    ec.write(FAN, ADVANCED_MODE, adv | (1 << fan))


QUIET = {  # temps stay, duty drops at low temperature; still 100% by 70 C
    "temps": [0, 40, 50, 60, 70, 127, 127, 127],
    "pwms":  [25, 35, 50, 75, 100, 100, 100, 100],
}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("action", choices=["show", "backup", "quiet", "curve", "restore"])
    ap.add_argument("fan", nargs="?", type=int)
    ap.add_argument("vals", nargs="*", type=int)
    ap.add_argument("file", nargs="?")
    a = ap.parse_args()

    STATE.mkdir(parents=True, exist_ok=True)
    ec = EC()
    try:
        if a.action == "show":
            show(ec)

        elif a.action == "backup":
            s = read_all(ec)
            p = STATE / f"backup-{time.strftime('%Y%m%d-%H%M%S')}.json"
            p.write_text(json.dumps(s))
            print(f"saved {p}")

        elif a.action == "quiet":
            s = read_all(ec)
            p = STATE / f"pre-quiet-{time.strftime('%Y%m%d-%H%M%S')}.json"
            p.write_text(json.dumps(s))
            print(f"backup: {p}")
            for f in range(8):
                if s["rpm"][f] == 0:
                    print(f"fan {f}: no tach, skipped")
                    continue
                write_curve(ec, f, QUIET["temps"], QUIET["pwms"])
                print(f"fan {f}: curve -> {QUIET['pwms']} + advanced mode")
            print("\nwaiting 10 s for the EC to re-evaluate...")
            time.sleep(10)
            show(ec)

        elif a.action == "curve":
            if a.fan is None or len(a.vals) != 16:
                sys.exit("usage: curve FAN t0..t7 p0..p7")
            write_curve(ec, a.fan, a.vals[:8], a.vals[8:])
            print(f"fan {a.fan} curve written")

        elif a.action == "restore":
            path = Path(a.file) if a.file else sorted(STATE.glob("backup-*.json"))[-1]
            d = json.loads(Path(path).read_text())
            for r, v in enumerate(d["acpi"]):
                ec.write(ACPI, r, v)
            for r, v in enumerate(d["smartfan"]):
                ec.write(FAN, r, v)
            print(f"restored both pages from {path}")
    finally:
        ec.close()


if __name__ == "__main__":
    main()
