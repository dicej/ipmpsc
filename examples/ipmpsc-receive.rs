#![deny(warnings)]

use clap::{App, Arg};
use failure::Error;
use ipmpsc::Receiver;

fn main() -> Result<(), Error> {
    let matches = App::new("ipmpsc-send")
        .about("ipmpsc sender example")
        .version(env!("CARGO_PKG_VERSION"))
        .author(env!("CARGO_PKG_AUTHORS"))
        .arg(
            Arg::with_name("map file")
                .help(
                    "File to use for shared memory ring buffer.  \
                     This file will be cleared if it already exists or created if it doesn't.",
                )
                .required(true),
        )
        .arg(
            Arg::with_name("zero copy")
                .long("zero-copy")
                .help("Use zero-copy deserialization"),
        )
        .get_matches();

    let map_file = matches.value_of("map file").unwrap();
    let mut rx = Receiver::from_path(map_file, 32 * 1024)?;
    let zero_copy = matches.is_present("zero copy");

    println!(
        "Ready!  Now run `cargo run --example ipmpsc-send {}` in another terminal.",
        map_file
    );

    loop {
        if zero_copy {
            println!("received {:?}", rx.zero_copy_context().recv::<&str>()?);
        } else {
            println!("received {:?}", rx.recv::<String>()?);
        }
    }
}
