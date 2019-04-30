#![deny(warnings)]

#[cfg(test)]
#[macro_use]
extern crate serde_derive;

use failure::{format_err, Error};
use memmap::MmapMut;
use serde::{Deserialize, Serialize};
use std::{
    cell::UnsafeCell,
    fs::OpenOptions,
    mem,
    sync::{
        atomic::{AtomicU32, Ordering::SeqCst},
        Arc,
    },
    time::{Duration, Instant},
};
use tempfile::NamedTempFile;

const BEGINNING: u32 = mem::size_of::<Header>() as u32;

const LONG_TIME_SECS: u64 = 60 * 60 * 24 * 365 * 1000;

macro_rules! nonzero {
    ($x:expr) => {{
        let x = $x;
        if x == 0 {
            Ok(())
        } else {
            Err(format_err!("{} failed: {}", stringify!($x), x))
        }
    }};
}

#[repr(C)]
struct Header {
    mutex: UnsafeCell<libc::pthread_mutex_t>,
    condition: UnsafeCell<libc::pthread_cond_t>,
    read: AtomicU32,
    write: AtomicU32,
}

impl Header {
    fn init(&self) -> Result<(), Error> {
        unsafe {
            let mut attr = mem::uninitialized::<libc::pthread_mutexattr_t>();
            nonzero!(libc::pthread_mutexattr_init(&mut attr))?;
            nonzero!(libc::pthread_mutexattr_setpshared(
                &mut attr,
                libc::PTHREAD_PROCESS_SHARED
            ))?;
            nonzero!(libc::pthread_mutex_init(self.mutex.get(), &attr))?;
            nonzero!(libc::pthread_mutexattr_destroy(&mut attr))?;

            let mut attr = mem::uninitialized::<libc::pthread_condattr_t>();
            nonzero!(libc::pthread_condattr_init(&mut attr))?;
            nonzero!(libc::pthread_condattr_setpshared(
                &mut attr,
                libc::PTHREAD_PROCESS_SHARED
            ))?;
            nonzero!(libc::pthread_cond_init(self.condition.get(), &attr))?;
            nonzero!(libc::pthread_condattr_destroy(&mut attr))?;
        }

        self.read.store(BEGINNING, SeqCst);
        self.write.store(BEGINNING, SeqCst);

        Ok(())
    }

    fn lock(&self) -> Result<Lock, Error> {
        unsafe {
            nonzero!(libc::pthread_mutex_lock(self.mutex.get()))?;
        }
        Ok(Lock(self))
    }

    fn notify(&self) -> Result<(), Error> {
        unsafe { nonzero!(libc::pthread_cond_signal(self.condition.get())) }
    }

    fn notify_all(&self) -> Result<(), Error> {
        unsafe { nonzero!(libc::pthread_cond_broadcast(self.condition.get())) }
    }
}

struct Lock<'a>(&'a Header);

impl<'a> Lock<'a> {
    fn wait(&self) -> Result<(), Error> {
        unsafe {
            nonzero!(libc::pthread_cond_wait(
                self.0.condition.get(),
                self.0.mutex.get()
            ))
        }
    }

    fn timed_wait(&self, timeout: Duration) -> Result<(), Error> {
        let timespec = libc::timespec {
            tv_sec: timeout.as_secs() as i64,
            tv_nsec: i64::from(timeout.subsec_nanos()),
        };

        unsafe {
            nonzero!(libc::pthread_cond_timedwait(
                self.0.condition.get(),
                self.0.mutex.get(),
                &timespec
            ))
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

pub struct Receiver {
    map: MmapMut,
    _file: NamedTempFile,
}

impl Receiver {
    fn header(&self) -> &Header {
        #[allow(clippy::cast_ptr_alignment)]
        unsafe {
            &*(self.map.as_ptr() as *const Header)
        }
    }

    fn seek(&self, position: u32) -> Result<(), Error> {
        let header = self.header();
        let _lock = header.lock()?;
        header.read.store(position, SeqCst);
        header.notify_all()
    }

    pub fn try_recv<T>(&self) -> Result<Option<T>, Error>
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

    fn try_recv_0<'a, T: Deserialize<'a>>(&'a self) -> Result<Option<(T, u32)>, Error> {
        let header = self.header();

        let mut read = header.read.load(SeqCst);
        let write = header.write.load(SeqCst);

        Ok(loop {
            if write != read {
                let buffer = self.map.as_ref();
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
                    let _lock = header.lock()?;
                    header.read.store(read, SeqCst);
                    header.notify_all()?;
                } else {
                    return Err(format_err!("corrupt ring buffer"));
                }
            } else {
                break None;
            }
        })
    }

    pub fn recv<T>(&self) -> Result<T, Error>
    where
        T: for<'de> Deserialize<'de>,
    {
        self.recv_timeout(Duration::from_secs(LONG_TIME_SECS))
            .map(Option::unwrap)
    }

    pub fn recv_timeout<T>(&self, timeout: Duration) -> Result<Option<T>, Error>
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

    /// Perform zero-copy deserialization.  Any byte or string
    /// references in the deserialized value will refer directly to
    /// the ring buffer maintained by this channel.
    ///
    /// NOTE CAREFULLY: Any references in the deserialized value will
    /// become invalid once the callback returns since the space they
    /// occupy will be overwritten by new messages.  Any data to be
    /// retained must be copied out of those the references before the
    /// callback returns.
    ///
    // TODO: is there a way to enforce the above requirement and
    // thereby make this method safe?
    pub unsafe fn recv_timeout_zero_copy<'a, T: Deserialize<'a>>(
        &'a self,
        timeout: Duration,
        callback: impl FnOnce(T),
    ) -> Result<bool, Error> {
        Ok(
            if let Some((value, position)) = self.recv_timeout_0(timeout)? {
                (callback)(value);
                self.seek(position)?;

                true
            } else {
                false
            },
        )
    }

    fn recv_timeout_0<'a, T: Deserialize<'a>>(
        &'a self,
        timeout: Duration,
    ) -> Result<Option<(T, u32)>, Error> {
        loop {
            if let Some(value_and_position) = self.try_recv_0()? {
                return Ok(Some(value_and_position));
            }

            let header = self.header();

            let read = header.read.load(SeqCst);

            let mut now = Instant::now();
            let deadline = now + timeout;

            let lock = header.lock()?;
            while read == header.write.load(SeqCst) {
                if deadline > now {
                    lock.timed_wait(deadline - now)?;
                    now = Instant::now();
                } else {
                    return Ok(None);
                }
            }
        }
    }
}

pub fn channel(size_in_bytes: u32) -> Result<(String, Receiver), Error> {
    let file = NamedTempFile::new()?;

    file.as_file()
        .set_len(u64::from(BEGINNING + size_in_bytes))?;

    let map = unsafe {
        let map = MmapMut::map_mut(file.as_file())?;

        #[allow(clippy::cast_ptr_alignment)]
        (*(map.as_ptr() as *const Header)).init()?;
        map
    };

    Ok((
        file.path()
            .to_str()
            .ok_or_else(|| format_err!("unable to represent path as string"))?
            .to_owned(),
        Receiver { map, _file: file },
    ))
}

#[derive(Clone)]
pub struct Sender {
    map: Arc<UnsafeCell<MmapMut>>,
}

unsafe impl Sync for Sender {}

unsafe impl Send for Sender {}

impl Sender {
    pub fn send(&self, value: &impl Serialize) -> Result<(), Error> {
        #[allow(clippy::cast_ptr_alignment)]
        let header = unsafe { &*((*self.map.get()).as_ptr() as *const Header) };

        let size = bincode::serialized_size(value)? as u32;

        if size == 0 {
            return Err(format_err!("zero sized values not supported"));
        }

        let map_len = unsafe { (*self.map.get()).len() };

        if (BEGINNING + size + 8) as usize > map_len {
            return Err(format_err!("message too large for ring buffer"));
        }

        let lock = header.lock()?;
        let mut write = header.write.load(SeqCst);
        loop {
            let read = header.read.load(SeqCst);

            if write >= read {
                if (write + size + 8) as usize <= map_len {
                    break;
                } else if read != BEGINNING {
                    assert!(write > BEGINNING);

                    unsafe {
                        bincode::serialize_into(
                            &mut (*self.map.get())[write as usize..(write + 4) as usize],
                            &0_u32,
                        )?;
                    }
                    write = BEGINNING;
                    header.write.store(write, SeqCst);
                    header.notify()?;
                    continue;
                }
            } else if write + size + 8 <= read {
                break;
            }

            lock.wait()?;
        }

        let start = write + 4;
        unsafe {
            bincode::serialize_into(
                &mut (*self.map.get())[write as usize..start as usize],
                &size,
            )?;
        }

        let end = start + size;
        unsafe {
            bincode::serialize_into(&mut (*self.map.get())[start as usize..end as usize], value)?;
        }

        header.write.store(end, SeqCst);

        header.notify()?;

        Ok(())
    }
}

pub fn sender(channel: &str) -> Result<Sender, Error> {
    let file = OpenOptions::new().read(true).write(true).open(channel)?;
    let map = unsafe { MmapMut::map_mut(&file)? };

    Ok(Sender {
        map: Arc::new(UnsafeCell::new(map)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::{arbitrary::any, collection::vec, prop_assume, proptest, strategy::Strategy};
    use std::thread;

    #[derive(Debug)]
    struct Case {
        channel_size: u32,
        data: Vec<Vec<u8>>,
    }

    impl Case {
        fn run(&self) -> Result<(), Error> {
            let (name, rx) = channel(self.channel_size)?;

            let expected = self.data.clone();
            let receiver_thread = thread::spawn(move || -> Result<(), Error> {
                for item in &expected {
                    let received = rx.recv::<Vec<u8>>()?;
                    assert_eq!(item, &received);
                }

                Ok(())
            });

            let tx = sender(&name)?;

            for item in &self.data {
                tx.send(item)?;
            }

            receiver_thread
                .join()
                .map_err(|e| format_err!("{:?}", e))??;

            Ok(())
        }
    }

    fn arb_case() -> impl Strategy<Value = Case> {
        (32_u32..1024).prop_flat_map(|channel_size| {
            vec(vec(any::<u8>(), 0..(channel_size as usize - 24)), 1..1024)
                .prop_map(move |data| Case { channel_size, data })
        })
    }

    #[test]
    fn simple_case() -> Result<(), Error> {
        Case {
            channel_size: 1024,
            data: (0..1024)
                .map(|_| (0_u8..101).collect::<Vec<_>>())
                .collect::<Vec<_>>(),
        }
        .run()
    }

    #[test]
    fn zero_copy() -> Result<(), Error> {
        #[derive(Serialize, Deserialize, Eq, PartialEq, Debug)]
        struct Foo<'a> {
            borrowed_str: &'a str,
            borrowed_bytes: &'a [u8],
        }

        let sent = Foo {
            borrowed_str: "hi",
            borrowed_bytes: &[0, 1, 2, 3],
        };

        let (name, rx) = channel(256)?;
        let tx = sender(&name)?;

        tx.send(&sent)?;

        unsafe {
            rx.recv_timeout_zero_copy(Duration::from_secs(LONG_TIME_SECS), |received| {
                assert_eq!(sent, received);
            })?;
        }

        Ok(())
    }

    proptest! {
        #[test]
        fn arbitrary_case(case in arb_case()) {
            let result = case.run();
            prop_assume!(result.is_ok(), "error: {:?}", result.unwrap_err());
        }
    }
}
