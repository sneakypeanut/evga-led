#!/usr/bin/env python3
"""READ-ONLY probe of the EVGA Z790 fan data on the Super I/O EC.

Reverse engineered from ELEET X1 1.0.16 SmartFan.dll:
  EC access: I/O ports 0x4E/0x4F, index register 0x2E, data register 0x2F
    write 0x2E to 0x4E, write <reg> to 0x4F   -> selects an EC register
    write 0x2F to 0x4E                        -> selects its data byte
    read/write 0x4F
  ECReg: Reg=0x10, Page=0x11, Data=0x12
  ECPage: AcpiRam=0xC4, SmartFan=0xC7, SmartFanRom=0xD1

  Page 0xC4 (AcpiRamProtocol):  VendorID@0(u16) DeviceID@2(u16) ver@4..9
                                FanControlPWMs@144 (2 bytes/fan: FanControl,PWM)
                                FanSpeeds@160 (u16 LE per fan)  FanMode@176 (u16)
  Page 0xC7 (SmartFanProtocol): FanLevel@0(8) FanTolerance@8(8)
                                Fans@16 (8 x [8 temps][8 pwms]) = 128 bytes
                                Fan_Step_Modes@144(8) Current_PWM@152(8)
                                Reserved@160(56) AdvancedMode@216
"""
import errno
import os

PORT = "/dev/port"
IDX_REG, DAT_REG = 0x2E, 0x2F
PORT_IDX, PORT_DAT = 0x4E, 0x4F
EREG_REG, EREG_PAGE, EREG_DATA = 0x10, 0x11, 0x12
PAGE_ACPI, PAGE_FAN = 0xC4, 0xC7


class EC:
    def __init__(self, path=PORT):
        self.fd = os.open(path, os.O_RDWR)

    def _wr(self, port, val):
        os.pwrite(self.fd, bytes([val & 0xFF]), port)

    def _rd(self, port):
        return os.pread(self.fd, 1, port)[0]

    def _reg(self, ecreg, value=None):
        self._wr(PORT_IDX, IDX_REG); self._wr(PORT_DAT, ecreg)
        self._wr(PORT_IDX, DAT_REG)
        if value is None:
            return self._rd(PORT_DAT)
        self._wr(PORT_DAT, value & 0xFF)
        return None

    def read(self, page, reg):
        self._reg(EREG_PAGE, page)
        self._reg(EREG_REG, reg)
        return self._reg(EREG_DATA)

    def read_block(self, page, start, length):
        return [self.read(page, start + i) for i in range(length)]

    def close(self):
        os.close(self.fd)


def main():
    ec = EC()
    try:
        ident = ec.read_block(PAGE_ACPI, 0, 10)
        vendor = ident[0] | (ident[1] << 8)
        device = ident[2] | (ident[3] << 8)
        print(f"AcpiRam page: VendorID=0x{vendor:04X} DeviceID=0x{device:04X} "
              f"main={ident[4]} sub={ident[5]} adc={ident[6]}/{ident[7]} fan={ident[8]}/{ident[9]}")
        if vendor == 0xFFFF or (vendor == 0 and device == 0):
            print("  -> no valid EC data at these ports (expect 0x3842 for EVGA)")

        print("\n--- per-fan state (page 0xC4) ---")
        speeds = ec.read_block(PAGE_ACPI, 160, 16)
        pwmcfg = ec.read_block(PAGE_ACPI, 144, 16)
        print(f"{'fan':>3} {'control':>8} {'PWM':>5} {'RPM':>6}")
        for f in range(8):
            rpm = speeds[f * 2] | (speeds[f * 2 + 1] << 8)
            ctrl = pwmcfg[f * 2]
            pwm = pwmcfg[f * 2 + 1]
            print(f"{f:>3} {ctrl:>8} {pwm:>5} {rpm:>6}")
        fanmode = ec.read_block(PAGE_ACPI, 176, 2)
        print(f"FanMode = {fanmode[0] | (fanmode[1] << 8)}")

        print("\n--- curves (page 0xC7) ---")
        adv = ec.read(PAGE_FAN, 216)
        cur_pwm = ec.read_block(PAGE_FAN, 152, 8)
        step = ec.read_block(PAGE_FAN, 144, 8)
        levels = ec.read_block(PAGE_FAN, 0, 8)
        tol = ec.read_block(PAGE_FAN, 8, 8)
        print(f"AdvancedMode={adv}  Current_PWM={cur_pwm}  StepModes={step}")
        print(f"FanLevel={levels}  FanTolerance={tol}")
        for f in range(8):
            base = 16 + f * 16
            blk = ec.read_block(PAGE_FAN, base, 16)
            temps, pwms = blk[:8], blk[8:]
            print(f"  fan {f}: temps={temps} pwms={pwms}")
    finally:
        ec.close()


if __name__ == "__main__":
    main()
