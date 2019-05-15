# ipmpsc

Inter-process Multiple Producer, Single Consumer Channels for Rust

## Summary

This project provides a type-safe, high-performance inter-process channel implementation based on a shared memory ring buffer.  It uses [bincode](https://github.com/TyOverby/bincode) for (de)serialization, including zero-copy deserialization.

## Platform Support

The current implementation should work on any POSIX-compatible OS.  It's been tested on Linux and Android.  Windows support is planned but has not been started yet.

## Security

The ring buffer is backed by a shared memory-mapped file, which means any process with access to that file can read from or write to it depending on its privileges.  This may or may not be acceptable depending on the security needs of your application and the environment in which it runs.
