#!/usr/bin/env python3
"""Find the PWM floor at which fan N keeps spinning, then set a safe curve above it."""
import sys, time
sys.path.insert(0, "/data")
from fan_ctl import EC, read_all, write_curve, show, CURVE_BASE, CURVE_LEN  # noqa

FAN = int(sys.argv[1]) if len(sys.argv) > 1 else 7
ec = EC()
try:
    temps = [0, 40, 50, 60, 70, 127, 127, 127]
    print(f"fan {FAN}: sweeping duty to find the stall floor")
    print(f"{'duty':>5} {'RPM':>6}")
    floor = None
    for duty in (20, 25, 30, 35, 40, 45, 50):
        pwms = [duty, duty + 5, duty + 15, duty + 30, 100, 100, 100, 100]
        write_curve(ec, FAN, temps, pwms)
        time.sleep(5)
        s = read_all(ec)
        rpm = s["rpm"][FAN]
        print(f"{duty:>5} {rpm:>6}")
        if rpm > 200:
            floor = duty
            break
    print(f"\nfirst duty that spins: {floor}")
    # settle on a curve whose low end sits comfortably above the floor
    base = max(floor + 10, 35) if floor else 45
    temps = [0, 40, 50, 60, 70, 127, 127, 127]
    pwms = [base, base + 10, base + 25, 85, 100, 100, 100, 100]
    write_curve(ec, FAN, temps, pwms)
    time.sleep(6)
    s = read_all(ec)
    print(f"settled curve: temps={temps}")
    print(f"               pwms ={pwms}  -> RPM={s['rpm'][FAN]}")
finally:
    ec.close()
