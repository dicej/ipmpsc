//! Inter-Process Multiple Producer, Single Consumer Channels for Rust
//!
//! This library provides a type-safe, high-performance inter-process channel implementation based on a shared
//! memory ring buffer.  It uses [bincode](https://github.com/TyOverby/bincode) for (de)serialization, including
//! zero-copy deserialization, making it ideal for messages with large `&str` or `&[u8]` fields.  And it has a name
//! that rolls right off the tongue.

#![deny(warnings)]

use memmap::MmapMut;
use native::{Buffer, Header, View};
use serde::{Deserialize, Serialize};
use std::{
    cell::UnsafeCell,
    fs::{File, OpenOptions},
    mem,
    sync::{atomic::Ordering::SeqCst, Arc},
    time::{Duration, Instant},
};
use tempfile::NamedTempFile;
use thiserror::Error as ThisError;

#[cfg(unix)]
mod posix;

#[cfg(unix)]
use posix as native;

mod bitmask;

#[cfg(windows)]
mod windows;

#[cfg(windows)]
use windows as native;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub const GIT_COMMIT_SHA_SHORT: &str = env!("VERGEN_SHA_SHORT");

const BEGINNING: u32 = mem::size_of::<Header>() as u32;

#[derive(ThisError, Debug)]
pub enum Error {
    /// Error indicating that the caller has attempted to read more than one message from a given
    /// [`ZeroCopyContext`](struct.ZeroCopyContext.html).
    #[error("A ZeroCopyContext may only be used to receive one message")]
    AlreadyReceived,

    /// Error indicating that the caller attempted to send a message of zero serialized size, which is not
    /// supported.
    #[error("Serialized size of message is zero")]
    ZeroSizedMessage,

    /// Error indicating that the caller attempted to send a message of serialized size greater than the ring
    /// buffer capacity.
    #[error("Serialized size of message is too large for ring buffer")]
    MessageTooLarge,

    /// Error indicating the the maximum number of simultaneous senders has been exceeded.
    #[error("Too many simultaneous senders")]
    TooManySenders,

    /// Implementation-specific runtime failure (e.g. a libc mutex error).
    #[error("{0}")]
    Runtime(String),

    /// Indicates the provided path contains null characters.
    #[error(transparent)]
    NulError(#[from] std::ffi::NulError),

    /// Implementation-specific runtime I/O failure (e.g. filesystem error).
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// Wrapped bincode error encountered during (de)serialization.
    #[error(transparent)]
    Bincode(#[from] bincode::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

fn forever() -> Duration {
    Duration::from_secs(u64::MAX)
}

fn map(file: &File) -> Result<MmapMut> {
    unsafe {
        let map = MmapMut::map_mut(&file)?;

        #[allow(clippy::cast_ptr_alignment)]
        (*(map.as_ptr() as *const Header)).init()?;

        Ok(map)
    }
}

/// Represents a file-backed shared memory ring buffer, suitable for constructing a
/// [`Receiver`](struct.Receiver.html) or [`Sender`](struct.Sender.html).
///
/// Note that it is possible to create multiple [`SharedRingBuffer`](struct.SharedRingBuffer.html)s for a given
/// path in a single process, but it is much more efficient to clone an exisiting instance than construct one from
/// scratch using one of the constructors.
#[derive(Clone)]
pub struct SharedRingBuffer(View);

unsafe impl Sync for SharedRingBuffer {}

unsafe impl Send for SharedRingBuffer {}

impl SharedRingBuffer {
    /// Creates a new [`SharedRingBuffer`](struct.SharedRingBuffer.html) backed by a file with the specified name.
    ///
    /// The file will be created if it does not already exist or truncated otherwise.
    ///
    /// Once this function completes successfully, the same path may be used to create one or more corresponding
    /// instances in other processes using the [`SharedRingBuffer::open`](struct.SharedRingBuffer.html#method.open)
    /// method.
    pub fn create(path: &str, size_in_bytes: u32) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;

        file.set_len(u64::from(BEGINNING + size_in_bytes))?;

        Ok(Self(View::try_new(Arc::new(UnsafeCell::new(
            Buffer::try_new(path, map(&file)?, None)?,
        )))?))
    }

    /// Creates a new [`SharedRingBuffer`](struct.SharedRingBuffer.html) backed by a temporary file which will be
    /// deleted when the [`SharedRingBuffer`](struct.SharedRingBuffer.html) is dropped.
    ///
    /// The name of the file is returned along with the [`SharedRingBuffer`](struct.SharedRingBuffer.html) and may
    /// be used to create one or more corresponding instances in other processes using the
    /// [`SharedRingBuffer::open`](struct.SharedRingBuffer.html#method.open) method.
    pub fn create_temp(size_in_bytes: u32) -> Result<(String, Self)> {
        let file = NamedTempFile::new()?;

        file.as_file()
            .set_len(u64::from(BEGINNING + size_in_bytes))?;

        let path = file
            .path()
            .to_str()
            .ok_or_else(|| Error::Runtime("unable to represent path as string".into()))?
            .to_owned();

        let map = map(file.as_file())?;

        Ok((
            path.to_owned(),
            Self(View::try_new(Arc::new(UnsafeCell::new(Buffer::try_new(
                &path,
                map,
                Some(file),
            )?)))?),
        ))
    }

    /// Creates a new [`SharedRingBuffer`](struct.SharedRingBuffer.html) backed by a file with the specified name.
    ///
    /// The file must already exist and have been initialized by a call to
    /// [`SharedRingBuffer::create`](struct.SharedRingBuffer.html#method.create) or
    /// [`SharedRingBuffer::create_temp`](struct.SharedRingBuffer.html#method.create_temp).
    pub fn open(path: &str) -> Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let map = unsafe { MmapMut::map_mut(&file)? };

        Ok(Self(View::try_new(Arc::new(UnsafeCell::new(
            Buffer::try_new(path, map, None)?,
        )))?))
    }
}

/// Represents the receiving end of an inter-process channel, capable of receiving any message type implementing
/// [`serde::Deserialize`](https://docs.serde.rs/serde/trait.Deserialize.html).
pub struct Receiver(SharedRingBuffer);

impl Receiver {
    /// Constructs a [`Receiver`](struct.Receiver.html) from the specified
    /// [`SharedRingBuffer`](struct.SharedRingBuffer.html)
    pub fn new(buffer: SharedRingBuffer) -> Self {
        Self(buffer)
    }

    fn seek(&self, position: u32) -> Result<()> {
        let buffer = self.0 .0.buffer();
        let mut lock = buffer.lock()?;
        buffer.header().read.store(position, SeqCst);
        lock.notify_all()
    }

    /// Attempt to read a message without blocking.
    ///
    /// This will return `Ok(None)` if there are no messages immediately available.
    pub fn try_recv<T>(&self) -> Result<Option<T>>
    where
        T: for<'de> Deserialize<'de>,
    {
        Ok(if let Some((value, position)) = self.try_recv_0()? {
            self.seek(position)?;

            Some(value)
        } else {
            None
        })
    }

    fn try_recv_0<'a, T: Deserialize<'a>>(&'a self) -> Result<Option<(T, u32)>> {
        let result;

        {
            let buffer = self.0 .0.buffer();
            let map = buffer.map();

            let mut read = buffer.header().read.load(SeqCst);
            let write = buffer.header().write.load(SeqCst);

            result = loop {
                if write != read {
                    let slice = map.as_ref();
                    let start = read + 4;
                    let size = bincode::deserialize::<u32>(&slice[read as usize..start as usize])?;
                    if size > 0 {
                        let end = start + size;
                        break Ok(Some((
                            bincode::deserialize(&slice[start as usize..end as usize])?,
                            end,
                        )));
                    } else if write < read {
                        read = BEGINNING;
                        let mut lock = buffer.lock()?;
                        buffer.header().read.store(read, SeqCst);
                        lock.notify_all()?;
                    } else {
                        break Err(Error::Runtime("corrupt ring buffer".into()));
                    }
                } else {
                    break Ok(None);
                }
            };
        }

        result
    }

    /// Attempt to read a message, blocking if necessary until one becomes available.
    pub fn recv<T>(&self) -> Result<T>
    where
        T: for<'de> Deserialize<'de>,
    {
        self.recv_timeout(forever()).map(Option::unwrap)
    }

    /// Attempt to read a message, blocking for up to the specified duration if necessary until one becomes
    /// available.
    pub fn recv_timeout<T>(&self, timeout: Duration) -> Result<Option<T>>
    where
        T: for<'de> Deserialize<'de>,
    {
        Ok(
            if let Some((value, position)) = self.recv_timeout_0(timeout)? {
                self.seek(position)?;

                Some(value)
            } else {
                None
            },
        )
    }

    /// Borrows this receiver for deserializing a message with references that refer directly to this
    /// [`Receiver`](struct.Receiver.html)'s ring buffer rather than copying out of it.
    ///
    /// Because those references refer directly to the ring buffer, the read pointer cannot be advanced until the
    /// lifetime of those references ends.
    ///
    /// To ensure the above, the following rules apply:
    ///
    /// 1. The underlying [`Receiver`](struct.Receiver.html) cannot be used while a
    /// [`ZeroCopyContext`](struct.ZeroCopyContext.html) borrows it (enforced at compile time).
    ///
    /// 2. References in a message deserialized using a given [`ZeroCopyContext`](struct.ZeroCopyContext.html)
    /// cannot outlive that instance (enforced at compile time).
    ///
    /// 3. A given [`ZeroCopyContext`](struct.ZeroCopyContext.html) can only be used to deserialize a single
    /// message before it must be discarded since the read pointer is advanced only when the instance is dropped
    /// (enforced at run time).
    pub fn zero_copy_context(&mut self) -> ZeroCopyContext {
        ZeroCopyContext {
            receiver: self,
            position: None,
        }
    }

    fn recv_timeout_0<'a, T: Deserialize<'a>>(
        &'a self,
        timeout: Duration,
    ) -> Result<Option<(T, u32)>> {
        let mut deadline = None;
        loop {
            if let Some(value_and_position) = self.try_recv_0()? {
                return Ok(Some(value_and_position));
            }

            let buffer = self.0 .0.buffer();

            let mut now = Instant::now();
            deadline = deadline.or_else(|| {
                if timeout == forever() {
                    None
                } else {
                    Some(now + timeout)
                }
            });

            let read = buffer.header().read.load(SeqCst);

            let mut lock = buffer.lock()?;
            while read == buffer.header().write.load(SeqCst) {
                if deadline.map(|deadline| deadline > now).unwrap_or(true) {
                    lock.timed_wait(
                        &self.0 .0,
                        deadline.map(|deadline| deadline - now).unwrap_or(forever()),
                    )?;

                    now = Instant::now();
                } else {
                    return Ok(None);
                }
            }
        }
    }
}

/// Borrows a [`Receiver`](struct.Receiver.html) for the purpose of doing zero-copy deserialization of messages
/// containing references.
///
/// An instance of this type may only be used to deserialize a single message before it is dropped because the
/// [`Drop`](https://doc.rust-lang.org/std/ops/trait.Drop.html) implementation is what advances the ring buffer
/// pointer.  Also, the borrowed [`Receiver`](struct.Receiver.html) may not be used directly while it is borrowed
/// by a [`ZeroCopyContext`](struct.ZeroCopyContext.html).
///
/// Use [`Receiver::zero_copy_context`](struct.Receiver.html#method.zero_copy_context) to create an instance.
pub struct ZeroCopyContext<'a> {
    receiver: &'a Receiver,
    position: Option<u32>,
}

impl<'a> ZeroCopyContext<'a> {
    /// Attempt to read a message without blocking.
    ///
    /// This will return `Ok(None)` if there are no messages immediately available.  It will return
    /// `Err(`[`Error::AlreadyReceived`](enum.Error.html#variant.AlreadyReceived)`))` if this instance has already
    /// been used to read a message.
    pub fn try_recv<'b, T: Deserialize<'b>>(&'b mut self) -> Result<Option<T>> {
        if self.position.is_some() {
            Err(Error::AlreadyReceived)
        } else {
            Ok(
                if let Some((value, position)) = self.receiver.try_recv_0()? {
                    self.position = Some(position);
                    Some(value)
                } else {
                    None
                },
            )
        }
    }

    /// Attempt to read a message, blocking if necessary until one becomes available.
    ///
    /// This will return `Err(`[`Error::AlreadyReceived`](enum.Error.html#variant.AlreadyReceived)`))` if this
    /// instance has already been used to read a message.
    pub fn recv<'b, T: Deserialize<'b>>(&'b mut self) -> Result<T> {
        self.recv_timeout(forever()).map(Option::unwrap)
    }

    /// Attempt to read a message, blocking for up to the specified duration if necessary until one becomes
    /// available.
    ///
    /// This will return `Err(`[`Error::AlreadyReceived`](enum.Error.html#variant.AlreadyReceived)`))` if this
    /// instance has already been used to read a message.
    pub fn recv_timeout<'b, T: Deserialize<'b>>(
        &'b mut self,
        timeout: Duration,
    ) -> Result<Option<T>> {
        if self.position.is_some() {
            Err(Error::AlreadyReceived)
        } else {
            Ok(
                if let Some((value, position)) = self.receiver.recv_timeout_0(timeout)? {
                    self.position = Some(position);
                    Some(value)
                } else {
                    None
                },
            )
        }
    }
}

impl<'a> Drop for ZeroCopyContext<'a> {
    fn drop(&mut self) {
        if let Some(position) = self.position.take() {
            let _ = self.receiver.seek(position);
        }
    }
}

/// Represents the sending end of an inter-process channel.
#[derive(Clone)]
pub struct Sender(SharedRingBuffer);

impl Sender {
    /// Constructs a [`Sender`](struct.Sender.html) from the specified
    /// [`SharedRingBuffer`](struct.SharedRingBuffer.html)
    pub fn new(buffer: SharedRingBuffer) -> Self {
        Self(buffer)
    }

    /// Send the specified message, waiting for sufficient contiguous space to become available in the ring buffer
    /// if necessary.
    ///
    /// The serialized size of the message must be greater than zero or else this method will return
    /// `Err(`[`Error::ZeroSizedMessage`](enum.Error.html#variant.ZeroSizedMessage)`))`.  If the serialized size is
    /// greater than the ring buffer capacity, this method will return
    /// `Err(`[`Error::MessageTooLarge`](enum.Error.html#variant.MessageTooLarge)`))`.
    pub fn send(&self, value: &impl Serialize) -> Result<()> {
        self.send_0(value, false)
    }

    /// Send the specified message, waiting for the ring buffer to become completely empty first.
    ///
    /// This method is appropriate for sending time-sensitive messages where buffering would introduce undesirable
    /// latency.
    ///
    /// The serialized size of the message must be greater than zero or else this method will return
    /// `Err(`[`error::ZeroSizedMessage`](struct.Error.html#variant.ZeroSizedMessage)`))`.  If the serialized size
    /// is greater than the ring buffer capacity, this method will return
    /// `Err(`[`Error::MessageTooLarge`](struct.Error.html#variant.MessageTooLarge)`))`.
    pub fn send_when_empty(&self, value: &impl Serialize) -> Result<()> {
        self.send_0(value, true)
    }

    fn send_0(&self, value: &impl Serialize, wait_until_empty: bool) -> Result<()> {
        let buffer = self.0 .0.buffer();
        let map = self.0 .0.map_mut();

        let size = bincode::serialized_size(value)? as u32;

        if size == 0 {
            return Err(Error::ZeroSizedMessage);
        }

        let map_len = map.len();

        if (BEGINNING + size + 8) as usize > map_len {
            return Err(Error::MessageTooLarge);
        }

        let mut lock = buffer.lock()?;
        let mut write;
        loop {
            write = buffer.header().write.load(SeqCst);
            let read = buffer.header().read.load(SeqCst);

            if write == read || (write > read && !wait_until_empty) {
                if (write + size + 8) as usize <= map_len {
                    break;
                } else if read != BEGINNING {
                    assert!(write > BEGINNING);

                    bincode::serialize_into(
                        &mut map[write as usize..(write + 4) as usize],
                        &0_u32,
                    )?;
                    write = BEGINNING;
                    buffer.header().write.store(write, SeqCst);
                    lock.notify_all()?;
                    continue;
                }
            } else if write + size + 8 <= read && !wait_until_empty {
                break;
            }

            lock.wait(&self.0 .0)?;
        }

        let start = write + 4;
        bincode::serialize_into(&mut map[write as usize..start as usize], &size)?;

        let end = start + size;
        bincode::serialize_into(&mut map[start as usize..end as usize], value)?;

        buffer.header().write.store(end, SeqCst);
        lock.notify_all()?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{anyhow, Result};
    use proptest::{arbitrary::any, collection::vec, prop_assume, proptest, strategy::Strategy};
    use std::thread;

    #[derive(Debug)]
    struct Case {
        channel_size: u32,
        data: Vec<Vec<u8>>,
        sender_count: u32,
    }

    impl Case {
        fn run(&self) -> Result<()> {
            let (name, buffer) = SharedRingBuffer::create_temp(self.channel_size)?;
            let rx = Receiver::new(buffer);

            let receiver_thread = if self.sender_count == 1 {
                // Only one sender means we can expect to receive in a predictable order:
                let expected = self.data.clone();
                thread::Builder::new()
                    .name("receiver".into())
                    .spawn(move || -> Result<()> {
                        for item in &expected {
                            let received = rx.recv::<Vec<u8>>()?;
                            assert_eq!(item, &received);
                        }

                        Ok(())
                    })?
            } else {
                // Multiple senders mean we'll receive in an unpredictable order, so just verify we receive the
                // expected number of messages:
                let expected = self.data.len() * self.sender_count as usize;
                thread::Builder::new()
                    .name("receiver".into())
                    .spawn(move || -> Result<()> {
                        for _ in 0..expected {
                            rx.recv::<Vec<u8>>()?;
                        }
                        Ok(())
                    })?
            };

            let tx = Sender::new(SharedRingBuffer::open(&name)?);

            let data = Arc::new(self.data.clone());
            let sender_threads = (0..self.sender_count)
                .map(move |i| {
                    thread::Builder::new().name(format!("sender-{}", i)).spawn({
                        let tx = tx.clone();
                        let data = data.clone();
                        move || -> Result<()> {
                            for item in data.as_ref() {
                                tx.send(item)?;
                            }

                            Ok(())
                        }
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;

            for thread in sender_threads {
                thread.join().map_err(|e| anyhow!("{:?}", e))??;
            }

            receiver_thread.join().map_err(|e| anyhow!("{:?}", e))??;

            Ok(())
        }
    }

    fn arb_case() -> impl Strategy<Value = Case> {
        ((32_u32..1024), (1_u32..5)).prop_flat_map(|(channel_size, sender_count)| {
            vec(vec(any::<u8>(), 0..(channel_size as usize - 24)), 1..1024).prop_map(move |data| {
                Case {
                    channel_size,
                    data,
                    sender_count,
                }
            })
        })
    }

    #[test]
    fn simple_case() -> Result<()> {
        Case {
            channel_size: 1024,
            data: (0..1024)
                .map(|_| (0_u8..101).collect::<Vec<_>>())
                .collect::<Vec<_>>(),
            sender_count: 1,
        }
        .run()
    }

    #[test]
    fn zero_copy() -> Result<()> {
        #[derive(Serialize, Deserialize, Eq, PartialEq, Debug)]
        struct Foo<'a> {
            borrowed_str: &'a str,
            #[serde(with = "serde_bytes")]
            borrowed_bytes: &'a [u8],
        }

        let sent = Foo {
            borrowed_str: "hi",
            borrowed_bytes: &[0, 1, 2, 3],
        };

        let (name, buffer) = SharedRingBuffer::create_temp(256)?;
        let mut rx = Receiver::new(buffer);
        let tx = Sender::new(SharedRingBuffer::open(&name)?);

        tx.send(&sent)?;
        tx.send(&42_u32)?;

        {
            let mut rx = rx.zero_copy_context();
            let received = rx.recv()?;

            assert_eq!(sent, received);
        }

        assert_eq!(42_u32, rx.recv()?);

        Ok(())
    }

    proptest! {
        #[test]
        fn arbitrary_case(case in arb_case()) {
            let result = thread::spawn(move || {
                let result = case.run();
                if let Err(e) = &result {
                    println!("\ntrouble: {:?}", e);
                } else if false {
                    print!(".");
                    std::io::Write::flush(&mut std::io::stdout())?;
                }
                result
            }).join().unwrap();

            prop_assume!(result.is_ok(), "error: {:?}", result.unwrap_err());
        }
    }
}
