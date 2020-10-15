use std::error::Error;
use vergen::ConstantsFlags;

fn main() -> Result<(), Box<dyn Error>> {
    vergen::generate_cargo_keys(ConstantsFlags::all())
}
