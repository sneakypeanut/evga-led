//! `evga-fan-probe` — read-only dump of the EVGA Z790 fan data.

use std::error::Error;

fn main() -> Result<(), Box<dyn Error>> {
    evga_z790_tools::fan::probe_main()
}
