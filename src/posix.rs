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
