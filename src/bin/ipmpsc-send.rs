#![deny(warnings)]

use failure::Error;

fn main() -> Result<(), Error> {
    let mut args = std::env::args();
    let _ = args.next();

    let tx = ipmpsc::sender(&args.next().unwrap())?;

    // todo: read messages from stdin instead
    for arg in args {
        tx.send(&arg)?;
    }

    Ok(())
}
