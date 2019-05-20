#![deny(warnings)]

use clap::{App, Arg};
use failure::Error;
use ipmpsc::Sender;
use std::io::{self, BufRead};

fn main() -> Result<(), Error> {
    let matches = App::new("ipmpsc-send")
        .about("ipmpsc sender example")
        .version(env!("CARGO_PKG_VERSION"))
        .author(env!("CARGO_PKG_AUTHORS"))
        .arg(
            Arg::with_name("map file")
                .help(
                    "File to use for shared memory ring buffer.  \
                     This should have already been created and initialized by the receiver.",
                )
                .required(true),
        )
        .get_matches();

    let map_file = matches.value_of("map file").unwrap();
    let tx = Sender::from_path(map_file)?;

    let mut buffer = String::new();
    let stdin = io::stdin();
    let mut handle = stdin.lock();

    println!("Ready!  Enter some lines of text to send them to the receiver.");

    while handle.read_line(&mut buffer)? > 0 {
        tx.send(&buffer)?;
        buffer.clear();
    }

    Ok(())
}
