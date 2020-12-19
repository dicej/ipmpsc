use crate::{Error, Result};
use memmap::MmapMut;
use std::{
    cell::UnsafeCell,
    mem::MaybeUninit,
    os::raw::c_long,
    sync::{
        atomic::{AtomicU32, Ordering::SeqCst},
        Arc,
    },
    time::{Duration, SystemTime},
};
use tempfile::NamedTempFile;

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

        self.read.store(crate::BEGINNING, SeqCst);
        self.write.store(crate::BEGINNING, SeqCst);

        Ok(())
    }
}

#[derive(Clone)]
pub struct View(Arc<UnsafeCell<Buffer>>);

impl View {
    pub fn try_new(buffer: Arc<UnsafeCell<Buffer>>) -> Result<Self> {
        Ok(View(buffer))
    }

    pub fn buffer(&self) -> &Buffer {
        unsafe { &*self.0.get() }
    }

    #[allow(clippy::mut_from_ref)]
    pub fn map_mut(&self) -> &mut MmapMut {
        unsafe { (*self.0.get()).map_mut() }
    }
}

pub struct Buffer {
    map: MmapMut,
    _file: Option<NamedTempFile>,
}

impl Buffer {
    pub fn try_new(_path: &str, map: MmapMut, file: Option<NamedTempFile>) -> Result<Self> {
        Ok(Buffer { map, _file: file })
    }

    pub fn header(&self) -> &Header {
        #[allow(clippy::cast_ptr_alignment)]
        unsafe {
            &*(self.map.as_ptr() as *const Header)
        }
    }

    pub fn lock(&self) -> Result<Lock> {
        Lock::try_new(self)
    }

    pub fn map(&self) -> &MmapMut {
        &self.map
    }

    pub fn map_mut(&mut self) -> &mut MmapMut {
        &mut self.map
    }
}

pub struct Lock<'a>(&'a Buffer);

impl<'a> Lock<'a> {
    pub fn try_new(buffer: &Buffer) -> Result<Lock> {
        unsafe {
            nonzero!(libc::pthread_mutex_lock(buffer.header().mutex.get()))?;
        }
        Ok(Lock(buffer))
    }

    pub fn notify_all(&mut self) -> Result<()> {
        unsafe {
            nonzero!(libc::pthread_cond_broadcast(
                self.0.header().condition.get()
            ))
        }
    }

    pub fn wait(&mut self, _view: &View) -> Result<()> {
        unsafe {
            nonzero!(libc::pthread_cond_wait(
                self.0.header().condition.get(),
                self.0.header().mutex.get()
            ))
        }
    }

    #[allow(clippy::cast_lossless)]
    pub fn timed_wait(&mut self, view: &View, timeout: Option<Duration>) -> Result<()> {
        if let Some(timeout) = timeout {
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
                    self.0.header().condition.get(),
                    self.0.header().mutex.get(),
                    &then
                )))
            }
        } else {
            self.wait(view)
        }
    }
}

impl<'a> Drop for Lock<'a> {
    fn drop(&mut self) {
        unsafe {
            libc::pthread_mutex_unlock(self.0.header().mutex.get());
        }
    }
}
