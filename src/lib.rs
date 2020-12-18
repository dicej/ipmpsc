//! Inter-Process Multiple Producer, Single Consumer Channels for Rust
//!
//! This library provides a type-safe, high-performance inter-process channel implementation based on a shared
//! memory ring buffer.  It uses [bincode](https://github.com/TyOverby/bincode) for (de)serialization, including
//! zero-copy deserialization, making it ideal for messages with large `&str` or `&[u8]` fields.  And it has a name
//! that rolls right off the tongue.

#![deny(warnings)]

use memmap::MmapMut;
use native::{Header, Lock, Monitor};
use serde::{Deserialize, Serialize};
use std::{
    cell::UnsafeCell,
    fs::{File, OpenOptions},
    mem,
    sync::{
        atomic::{AtomicU32, Ordering::SeqCst},
        Arc,
    },
    time::{Duration, Instant},
};
use tempfile::NamedTempFile;
use thiserror::Error as ThisError;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub const GIT_COMMIT_SHA_SHORT: &str = env!("VERGEN_SHA_SHORT");

const BEGINNING: u32 = mem::size_of::<Header>() as u32;

const VERBOSE: bool = false;

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

#[cfg(unix)]
mod native {
    use super::*;
    use std::{mem::MaybeUninit, os::raw::c_long, time::SystemTime};

    // libc::PTHREAD_PROCESS_SHARED doesn't exist for Android for some
    // reason, so we need to declare it ourselves:
    #[cfg(target_os = "android")]
    const PTHREAD_PROCESS_SHARED: i32 = 1;

    #[cfg(not(target_os = "android"))]
    const PTHREAD_PROCESS_SHARED: i32 = libc::PTHREAD_PROCESS_SHARED;

    macro_rules! nonzero {
        ($x:expr) => {{
            let x = $x;
            if x == 0 {
                Ok(())
            } else {
                Err(Error::Runtime(format!("{} failed: {}", stringify!($x), x)))
            }
        }};
    }

    #[repr(C)]
    pub struct Header {
        mutex: UnsafeCell<libc::pthread_mutex_t>,
        condition: UnsafeCell<libc::pthread_cond_t>,
        pub read: AtomicU32,
        pub write: AtomicU32,
    }

    impl Header {
        pub fn init(&self) -> Result<()> {
            unsafe {
                let mut attr = MaybeUninit::<libc::pthread_mutexattr_t>::uninit();
                nonzero!(libc::pthread_mutexattr_init(attr.as_mut_ptr()))?;
                nonzero!(libc::pthread_mutexattr_setpshared(
                    attr.as_mut_ptr(),
                    PTHREAD_PROCESS_SHARED
                ))?;
                nonzero!(libc::pthread_mutex_init(self.mutex.get(), attr.as_ptr()))?;
                nonzero!(libc::pthread_mutexattr_destroy(attr.as_mut_ptr()))?;

                let mut attr = MaybeUninit::<libc::pthread_condattr_t>::uninit();
                nonzero!(libc::pthread_condattr_init(attr.as_mut_ptr()))?;
                nonzero!(libc::pthread_condattr_setpshared(
                    attr.as_mut_ptr(),
                    PTHREAD_PROCESS_SHARED
                ))?;
                nonzero!(libc::pthread_cond_init(self.condition.get(), attr.as_ptr()))?;
                nonzero!(libc::pthread_condattr_destroy(attr.as_mut_ptr()))?;
            }

            self.read.store(BEGINNING, SeqCst);
            self.write.store(BEGINNING, SeqCst);

            Ok(())
        }
    }

    pub struct Monitor;

    impl Monitor {
        pub fn try_new(_path: &str) -> Result<Monitor> {
            Ok(Monitor)
        }
    }

    pub struct Lock<'a>(&'a Header);

    impl<'a> Lock<'a> {
        pub fn try_new(header: &Header, _monitor: &Monitor) -> Result<Lock> {
            unsafe {
                nonzero!(libc::pthread_mutex_lock(header.mutex.get()))?;
            }
            Ok(Lock(header))
        }

        pub fn notify_all(&self) -> Result<()> {
            unsafe { nonzero!(libc::pthread_cond_broadcast(self.0.condition.get())) }
        }

        pub fn wait(&self) -> Result<()> {
            unsafe {
                nonzero!(libc::pthread_cond_wait(
                    self.0.condition.get(),
                    self.0.mutex.get()
                ))
            }
        }

        #[allow(clippy::cast_lossless)]
        pub fn timed_wait(&self, timeout: Duration) -> Result<()> {
            if timeout == forever() {
                self.wait()
            } else {
                let then = SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap()
                    + timeout;

                let then = libc::timespec {
                    tv_sec: then.as_secs() as libc::time_t,
                    tv_nsec: then.subsec_nanos() as c_long,
                };

                let timeout_ok = |result| if result == libc::ETIMEDOUT { 0 } else { result };

                unsafe {
                    nonzero!(timeout_ok(libc::pthread_cond_timedwait(
                        self.0.condition.get(),
                        self.0.mutex.get(),
                        &then
                    )))
                }
            }
        }
    }

    impl<'a> Drop for Lock<'a> {
        fn drop(&mut self) {
            unsafe {
                libc::pthread_mutex_unlock(self.0.mutex.get());
            }
        }
    }
}

#[cfg(windows)]
mod native {
    use super::*;
    use std::{
        convert::TryInto,
        ffi::{CStr, CString},
        ptr, slice,
    };
    use winapi::{
        shared::{
            minwindef::{self, LPVOID, ULONG},
            winerror,
        },
        um::{
            errhandlingapi, handleapi, synchapi, winbase,
            winnt::{HANDLE, LPSTR},
        },
    };

    pub struct Defer<F: FnMut()>(F);

    impl<F: FnMut()> Drop for Defer<F> {
        fn drop(&mut self) {
            (self.0)();
        }
    }

    macro_rules! defer {
        ($e:expr) => {
            let _defer = Defer($e);
        };
    }

    fn get_last_error() -> String {
        unsafe {
            let error = errhandlingapi::GetLastError();

            if error == 0 {
                None
            } else {
                let mut buffer: LPSTR = ptr::null_mut();
                let size = winbase::FormatMessageA(
                    winbase::FORMAT_MESSAGE_ALLOCATE_BUFFER
                        | winbase::FORMAT_MESSAGE_FROM_SYSTEM
                        | winbase::FORMAT_MESSAGE_IGNORE_INSERTS,
                    ptr::null(),
                    error,
                    0,
                    &mut buffer as *mut _ as LPSTR,
                    0,
                    ptr::null_mut(),
                );

                if buffer.is_null() {
                    None
                } else {
                    defer!(|| {
                        winbase::LocalFree(buffer as LPVOID);
                    });

                    slice::from_raw_parts_mut(buffer, size as usize)[(size - 1) as usize] = 0;

                    CStr::from_ptr(buffer)
                        .to_str()
                        .ok()
                        .map(|s| s.trim().to_owned())
                }
            }
        }
        .unwrap_or_else(|| "unknown error".to_owned())
    }

    macro_rules! success {
        ($x:expr) => {{
            let x = $x;
            if x {
                Ok(())
            } else {
                Err(Error::Runtime(format!(
                    "{} failed: {}",
                    stringify!($x),
                    get_last_error()
                )))
            }
        }};
    }

    #[derive(Copy, Clone)]
    struct BitMask(u128);

    impl BitMask {
        fn capacity() -> u8 {
            (mem::size_of::<BitMask>() * 8) as u8
        }

        fn find_first_clear_bit(self) -> Result<u8> {
            for index in 0..BitMask::capacity() {
                if !self.get(index) {
                    return Ok(index);
                }
            }

            Err(Error::TooManySenders)
        }

        fn count(self) -> u8 {
            let mut count = 0;
            for index in 0..BitMask::capacity() {
                if self.get(index) {
                    count += 1;
                }
            }
            count
        }

        fn get(self, index: u8) -> bool {
            (self.0 & 1u128.checked_shl(index.into()).unwrap()) != 0
        }

        fn set(self, index: u8) -> BitMask {
            BitMask(self.0 | 1u128.checked_shl(index.into()).unwrap())
        }

        fn clear(&self, index: u8) -> BitMask {
            BitMask(self.0 & !(1u128.checked_shl(index.into()).unwrap()))
        }
    }

    #[repr(C)]
    pub struct Header {
        threads: UnsafeCell<BitMask>,
        waiters: UnsafeCell<BitMask>,
        pub read: AtomicU32,
        pub write: AtomicU32,
    }

    impl Header {
        pub fn init(&self) -> Result<()> {
            unsafe {
                ptr::write(self.threads.get(), BitMask(0));
                ptr::write(self.waiters.get(), BitMask(0));
            }
            self.read.store(BEGINNING, SeqCst);
            self.write.store(BEGINNING, SeqCst);

            Ok(())
        }
    }

    pub struct Monitor {
        mutex: HANDLE,
        semaphore: HANDLE,
    }

    impl Monitor {
        pub fn try_new(path: &str) -> Result<Monitor> {
            let path = path.replace(&['\\', ':', '~', '.'][..], "_");

            let mut monitor = Monitor {
                mutex: ptr::null_mut(),
                semaphore: ptr::null_mut(),
            };

            unsafe {
                let mutex_string = format!("{}-mutex", path);
                let mutex_name = CString::new(mutex_string.clone())?;

                monitor.mutex =
                    synchapi::CreateMutexA(ptr::null_mut(), minwindef::FALSE, mutex_name.as_ptr());

                if monitor.mutex.is_null() {
                    return Err(Error::Runtime(format!(
                        "CreateMutex for {} failed: {}",
                        mutex_string,
                        get_last_error()
                    )));
                }

                let semaphore_string = format!("{}-semaphore", path);
                let semaphore_name = CString::new(semaphore_string.clone())?;

                monitor.semaphore = winbase::CreateSemaphoreA(
                    ptr::null_mut(),
                    0,
                    BitMask::capacity().into(),
                    semaphore_name.as_ptr(),
                );

                if monitor.semaphore.is_null() {
                    return Err(Error::Runtime(format!(
                        "CreateSemaphore for {} failed: {}",
                        semaphore_string,
                        get_last_error()
                    )));
                }
            }

            Ok(monitor)
        }
    }

    impl Drop for Monitor {
        fn drop(&mut self) {
            unsafe {
                if !self.mutex.is_null() {
                    handleapi::CloseHandle(self.mutex);
                }

                if !self.semaphore.is_null() {
                    handleapi::CloseHandle(self.semaphore);
                }
            }
        }
    }

    pub struct Lock<'a> {
        locked: bool,
        header: &'a Header,
        monitor: &'a Monitor,
    }

    impl<'a> Lock<'a> {
        pub fn try_new(header: &'a Header, monitor: &'a Monitor) -> Result<Lock<'a>> {
            if VERBOSE {
                eprintln!(
                    "{} lock",
                    std::thread::current().name().unwrap_or("unknown")
                );
            }

            unsafe {
                success!(
                    winbase::WAIT_OBJECT_0
                        == synchapi::WaitForSingleObject(monitor.mutex, winbase::INFINITE)
                )?;
            }

            if VERBOSE {
                eprintln!(
                    "{} locked",
                    std::thread::current().name().unwrap_or("unknown")
                );
            }

            Ok(Lock {
                locked: true,
                header,
                monitor,
            })
        }

        fn do_wait(&mut self, milliseconds: ULONG) -> Result<()> {
            unsafe {
                let threads = ptr::read(self.header.threads.get());
                let index = threads.find_first_clear_bit()?;

                if VERBOSE {
                    eprintln!(
                        "{} enter wait; threads {} waiters {} index {}",
                        std::thread::current().name().unwrap_or("unknown"),
                        ptr::read(self.header.threads.get()).count(),
                        ptr::read(self.header.waiters.get()).count(),
                        index
                    );
                }

                ptr::write(self.header.threads.get(), threads.set(index));

                ptr::write(
                    self.header.waiters.get(),
                    ptr::read(self.header.waiters.get()).set(index),
                );

                ptr::write(self.header.threads.get(), threads.set(index));
                success!(minwindef::TRUE == synchapi::ReleaseMutex(self.monitor.mutex))?;

                self.locked = false;

                success!(matches!(
                    synchapi::WaitForSingleObject(self.monitor.semaphore, milliseconds),
                    winbase::WAIT_OBJECT_0 | winerror::WAIT_TIMEOUT
                ))?;

                success!(
                    winbase::WAIT_OBJECT_0
                        == synchapi::WaitForSingleObject(self.monitor.mutex, winbase::INFINITE)
                )?;

                self.locked = true;

                ptr::write(
                    self.header.waiters.get(),
                    ptr::read(self.header.waiters.get()).clear(index),
                );

                ptr::write(
                    self.header.threads.get(),
                    ptr::read(self.header.threads.get()).clear(index),
                );

                if VERBOSE {
                    eprintln!(
                        "{} exit wait; threads {} waiters {} index {}",
                        std::thread::current().name().unwrap_or("unknown"),
                        ptr::read(self.header.threads.get()).count(),
                        ptr::read(self.header.waiters.get()).count(),
                        index
                    );
                }
            }

            Ok(())
        }

        pub fn wait(&mut self) -> Result<()> {
            self.do_wait(winbase::INFINITE)
        }

        pub fn timed_wait(&mut self, timeout: Duration) -> Result<()> {
            self.do_wait(if timeout == forever() {
                winbase::INFINITE
            } else {
                timeout.as_millis().try_into().map_err(|_| {
                    Error::Runtime("unable to represent timeout in milliseconds as ULONG".into())
                })?
            })
        }

        pub fn notify_all(&self) -> Result<()> {
            unsafe {
                let count = ptr::read(self.header.waiters.get()).count();

                if VERBOSE {
                    eprintln!(
                        "{} notify_all threads {} waiters {}",
                        std::thread::current().name().unwrap_or("unknown"),
                        ptr::read(self.header.threads.get()).count(),
                        ptr::read(self.header.waiters.get()).count()
                    );
                }

                if 0 == count
                    || minwindef::TRUE
                        == synchapi::ReleaseSemaphore(
                            self.monitor.semaphore,
                            count.into(),
                            ptr::null_mut(),
                        )
                {
                    ptr::write(self.header.waiters.get(), BitMask(0));

                    Ok(())
                } else {
                    Err(Error::Runtime(format!(
                        "ReleaseSemaphore failed: {}",
                        get_last_error()
                    )))
                }
            }
        }
    }

    impl<'a> Drop for Lock<'a> {
        fn drop(&mut self) {
            if self.locked {
                if VERBOSE {
                    eprintln!(
                        "{} unlock",
                        std::thread::current().name().unwrap_or("unknown")
                    );
                }

                unsafe {
                    synchapi::ReleaseMutex(self.monitor.mutex);
                }
            }
        }
    }
}

fn map(file: &File) -> Result<MmapMut> {
    unsafe {
        let map = MmapMut::map_mut(&file)?;

        #[allow(clippy::cast_ptr_alignment)]
        (*(map.as_ptr() as *const Header)).init()?;

        Ok(map)
    }
}

struct RingBuffer {
    map: MmapMut,
    monitor: Monitor,
    _file: Option<NamedTempFile>,
}

/// Represents a file-backed shared memory ring buffer, suitable for constructing a
/// [`Receiver`](struct.Receiver.html) or [`Sender`](struct.Sender.html).
///
/// Note that it is possible to create multiple [`SharedRingBuffer`](struct.SharedRingBuffer.html)s for a given
/// path in a single process, but it is much more efficient to clone an exisiting instance than construct one from
/// scratch using one of the constructors.
#[derive(Clone)]
pub struct SharedRingBuffer {
    inner: Arc<UnsafeCell<RingBuffer>>,
}

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
    pub fn create(path: &str, size_in_bytes: u32) -> Result<SharedRingBuffer> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;

        file.set_len(u64::from(BEGINNING + size_in_bytes))?;

        Ok(SharedRingBuffer {
            inner: Arc::new(UnsafeCell::new(RingBuffer {
                map: map(&file)?,
                monitor: Monitor::try_new(path)?,
                _file: None,
            })),
        })
    }

    /// Creates a new [`SharedRingBuffer`](struct.SharedRingBuffer.html) backed by a temporary file which will be
    /// deleted when the [`SharedRingBuffer`](struct.SharedRingBuffer.html) is dropped.
    ///
    /// The name of the file is returned along with the [`SharedRingBuffer`](struct.SharedRingBuffer.html) and may
    /// be used to create one or more corresponding instances in other processes using the
    /// [`SharedRingBuffer::open`](struct.SharedRingBuffer.html#method.open) method.
    pub fn create_temp(size_in_bytes: u32) -> Result<(String, SharedRingBuffer)> {
        let file = NamedTempFile::new()?;

        file.as_file()
            .set_len(u64::from(BEGINNING + size_in_bytes))?;

        let path = file
            .path()
            .to_str()
            .ok_or_else(|| Error::Runtime("unable to represent path as string".into()))?;

        Ok((
            path.to_owned(),
            SharedRingBuffer {
                inner: Arc::new(UnsafeCell::new(RingBuffer {
                    map: map(file.as_file())?,
                    monitor: Monitor::try_new(path)?,
                    _file: Some(file),
                })),
            },
        ))
    }

    /// Creates a new [`SharedRingBuffer`](struct.SharedRingBuffer.html) backed by a file with the specified name.
    ///
    /// The file must already exist and have been initialized by a call to
    /// [`SharedRingBuffer::create`](struct.SharedRingBuffer.html#method.create) or
    /// [`SharedRingBuffer::create_temp`](struct.SharedRingBuffer.html#method.create_temp).
    pub fn open(path: &str) -> Result<SharedRingBuffer> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let map = unsafe { MmapMut::map_mut(&file)? };

        Ok(SharedRingBuffer {
            inner: Arc::new(UnsafeCell::new(RingBuffer {
                map,
                monitor: Monitor::try_new(path)?,
                _file: None,
            })),
        })
    }

    fn header(&self) -> &Header {
        #[allow(clippy::cast_ptr_alignment)]
        unsafe {
            &*((*self.inner.get()).map.as_ptr() as *const Header)
        }
    }

    fn monitor(&self) -> &Monitor {
        unsafe { &(*self.inner.get()).monitor }
    }
}

/// Represents the receiving end of an inter-process channel, capable of receiving any message type implementing
/// [`serde::Deserialize`](https://docs.serde.rs/serde/trait.Deserialize.html).
pub struct Receiver {
    buffer: SharedRingBuffer,
}

impl Receiver {
    /// Constructs a [`Receiver`](struct.Receiver.html) from the specified
    /// [`SharedRingBuffer`](struct.SharedRingBuffer.html)
    pub fn new(buffer: SharedRingBuffer) -> Self {
        Self { buffer }
    }

    fn seek(&self, position: u32) -> Result<()> {
        let header = self.buffer.header();
        let lock = Lock::try_new(header, self.buffer.monitor())?;
        header.read.store(position, SeqCst);
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
        let header = self.buffer.header();
        let monitor = self.buffer.monitor();
        let map = unsafe { &(*self.buffer.inner.get()).map };

        let mut read = header.read.load(SeqCst);
        let write = header.write.load(SeqCst);

        Ok(loop {
            if write != read {
                let buffer = map.as_ref();
                let start = read + 4;
                let size = bincode::deserialize::<u32>(&buffer[read as usize..start as usize])?;
                if size > 0 {
                    let end = start + size;
                    break Some((
                        bincode::deserialize(&buffer[start as usize..end as usize])?,
                        end,
                    ));
                } else if write < read {
                    read = BEGINNING;
                    let lock = Lock::try_new(header, monitor)?;
                    header.read.store(read, SeqCst);
                    lock.notify_all()?;
                } else {
                    return Err(Error::Runtime("corrupt ring buffer".into()));
                }
            } else {
                break None;
            }
        })
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

            let header = self.buffer.header();
            let monitor = self.buffer.monitor();

            let mut now = Instant::now();
            deadline = deadline.or_else(|| {
                if timeout == forever() {
                    None
                } else {
                    Some(now + timeout)
                }
            });

            let read = header.read.load(SeqCst);

            let mut lock = Lock::try_new(header, monitor)?;
            while read == header.write.load(SeqCst) {
                if deadline.map(|deadline| deadline > now).unwrap_or(true) {
                    lock.timed_wait(deadline.map(|deadline| deadline - now).unwrap_or(forever()))?;

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
pub struct Sender {
    buffer: SharedRingBuffer,
}

impl Sender {
    /// Constructs a [`Sender`](struct.Sender.html) from the specified
    /// [`SharedRingBuffer`](struct.SharedRingBuffer.html)
    pub fn new(buffer: SharedRingBuffer) -> Self {
        Self { buffer }
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
        let header = self.buffer.header();
        let monitor = self.buffer.monitor();
        let map = unsafe { &mut (*self.buffer.inner.get()).map };

        let size = bincode::serialized_size(value)? as u32;

        if size == 0 {
            return Err(Error::ZeroSizedMessage);
        }

        let map_len = map.len();

        if (BEGINNING + size + 8) as usize > map_len {
            return Err(Error::MessageTooLarge);
        }

        let mut lock = Lock::try_new(header, monitor)?;
        let mut write;
        loop {
            write = header.write.load(SeqCst);
            let read = header.read.load(SeqCst);

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
                    header.write.store(write, SeqCst);
                    lock.notify_all()?;
                    continue;
                }
            } else if write + size + 8 <= read && !wait_until_empty {
                break;
            }

            lock.wait()?;
        }

        let start = write + 4;
        bincode::serialize_into(&mut map[write as usize..start as usize], &size)?;

        let end = start + size;
        bincode::serialize_into(&mut map[start as usize..end as usize], value)?;

        header.write.store(end, SeqCst);
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
                    println!("trouble: {:?}", e);
                } else {
                    println!("yay!");
                }
                result
            }).join().unwrap();

            prop_assume!(result.is_ok(), "error: {:?}", result.unwrap_err());
        }
    }
}
