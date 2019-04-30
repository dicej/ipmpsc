#![deny(warnings)]

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
};
use tempfile::NamedTempFile;

const BEGINNING: u32 = mem::size_of::<Header>() as u32;

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
    pub fn try_recv<T>(&self) -> Result<Option<T>, Error>
    where
        T: for<'de> Deserialize<'de>,
    {
        #[allow(clippy::cast_ptr_alignment)]
        let header = unsafe { &*(self.map.as_ptr() as *const Header) };

        let mut read = header.read.load(SeqCst);
        let write = header.write.load(SeqCst);

        Ok(loop {
            if write != read {
                let buffer = self.map.as_ref();
                let start = read + 4;
                let size = bincode::deserialize::<u32>(&buffer[read as usize..start as usize])?;
                if size > 0 {
                    let end = start + size;
                    let value = bincode::deserialize(&buffer[start as usize..end as usize])?;
                    let _lock = header.lock()?;
                    header.read.store(end, SeqCst);
                    header.notify_all()?;
                    break Some(value);
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
        Ok(loop {
            if let Some(value) = self.try_recv()? {
                break value;
            }

            #[allow(clippy::cast_ptr_alignment)]
            let header = unsafe { &*(self.map.as_ptr() as *const Header) };

            let read = header.read.load(SeqCst);

            let lock = header.lock()?;
            while read == header.write.load(SeqCst) {
                lock.wait()?;
            }
        })
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

    proptest! {
        #[test]
        fn arbitrary_case(case in arb_case()) {
            let result = case.run();
            prop_assume!(result.is_ok(), "error: {:?}", result.unwrap_err());
        }
    }
}
