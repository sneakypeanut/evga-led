//! `evga-fan-sweep` — find a fan's stall floor, then settle a safe curve.

use std::error::Error;

fn main() -> Result<(), Box<dyn Error>> {
    evga_z790_tools::fan::sweep_main()
}
