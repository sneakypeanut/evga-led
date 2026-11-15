//! `evga-fan` — fan curves for the EVGA Z790 CLASSIFIED.

use std::error::Error;

fn main() -> Result<(), Box<dyn Error>> {
    evga_z790_tools::fan::run()
}
