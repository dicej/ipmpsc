# ipmpsc

Inter-Process Multiple Producer, Single Consumer Channels for Rust

[![Crates.io](https://img.shields.io/crates/v/ipmpsc)](https://crates.io/crates/ipmpsc)
[![Build Status](https://travis-ci.org/dicej/ipmpsc.svg?branch=master)](https://travis-ci.org/dicej/ipmpsc)
[![MIT licensed](https://img.shields.io/badge/license-MIT-blue.svg)](./LICENSE.md)

## Summary

This library provides a type-safe, high-performance inter-process
channel implementation based on a shared memory ring buffer.  It uses
[bincode](https://github.com/TyOverby/bincode) for (de)serialization,
including zero-copy deserialization, making it ideal for messages with
large `&str` or `&[u8]` fields.  And it has a name that rolls right
off the tongue.

## Examples

The examples directory contains a sender and receiver pair, which you
can run in separate terminals like so:

```bash
cargo run --example ipmpsc-receive -- --zero-copy /tmp/ipmpsc
```

```bash
cargo run --example ipmpsc-send -- /tmp/ipmpsc
```

Type some lines of text into the sender and observe that they are
printed by the receiver.  You can also run additional senders from
other terminals -- the receiver will receive messages from any of
them.

## Performance

`ipmpsc::Receiver::zero_copy_context`, used in combination with
[serde_bytes](https://github.com/serde-rs/bytes), is capable of
supporting very high bandwidth, low latency transfers
(e.g. uncompressed video frames).

## Security

The ring buffer is backed by a shared memory-mapped file, which means
any process with access to that file can read from or write to it
depending on its privileges.  This may or may not be acceptable
depending on the security needs of your application and the
environment in which it runs.

## Platform Support

The current implementation should work on Windows and any POSIX-compatible OS.
It's been tested on Linux, Windows and Android.

## Similar Projects

[ipc-channel](https://github.com/servo/ipc-channel) - mature and
robust IPC channels.  Does not yet support Android, multiple
simultaneous senders, or zero-copy deserialization.

[shared_memory](https://github.com/elast0ny/shared_memory-rs) -
low-level, cross-platform shared memory support.  May be used as the
basis for a ring-buffer based channel, but does not yet support
Android.
