use crate::{Error, Result};
use memmap2::MmapMut;
use std::{
    cell::UnsafeCell,
    mem::MaybeUninit,
    os::raw::c_long,
    sync::{
        atomic::{AtomicU32, Ordering::Relaxed},
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
    pub flags: AtomicU32,
    mutex: UnsafeCell<libc::pthread_mutex_t>,
    condition: UnsafeCell<libc::pthread_cond_t>,
    pub read: AtomicU32,
    pub write: AtomicU32,
}

impl Header {
    pub fn init(&self) -> Result<()> {
        self.flags.store(crate::flags(), Relaxed);

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

        self.read.store(crate::BEGINNING, Relaxed);
        self.write.store(crate::BEGINNING, Relaxed);

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

#[cfg(any(test, feature = "fork"))]
pub mod test {
    use anyhow::{anyhow, Result};
    use errno::errno;
    use std::{
        ffi::c_void,
        io::Write,
        os::raw::c_int,
        process,
        thread::{self, JoinHandle},
    };

    struct Descriptor(c_int);

    impl Descriptor {
        fn forward(&self, dst: &mut dyn Write) -> Result<()> {
            let mut buffer = [0u8; 1024];

            loop {
                let count =
                    unsafe { libc::read(self.0, buffer.as_mut_ptr() as *mut c_void, buffer.len()) };

                match count {
                    -1 => {
                        break Err(anyhow!("unable to read; errno: {}", errno()));
                    }
                    0 => {
                        break Ok(());
                    }
                    _ => {
                        dst.write_all(&buffer[..count as usize])?;
                    }
                }
            }
        }
    }

    impl Drop for Descriptor {
        fn drop(&mut self) {
            unsafe { libc::close(self.0) };
        }
    }

    fn pipe() -> Result<(Descriptor, Descriptor)> {
        let mut fds = [0; 2];
        if -1 == unsafe { libc::pipe(fds.as_mut_ptr()) } {
            return Err(anyhow!("pipe failed; errno: {}", errno()));
        }

        Ok((Descriptor(fds[0]), Descriptor(fds[1])))
    }

    pub fn fork<F: Send + 'static + FnOnce() -> Result<()>>(
        fun: F,
    ) -> Result<JoinHandle<Result<()>>> {
        let (_, out_tx) = pipe()?;
        let (err_rx, err_tx) = pipe()?;
        let (alive_rx, alive_tx) = pipe()?;

        match unsafe { libc::fork() } {
            -1 => Err(anyhow!("fork failed; errno: {}", errno())),
            0 => {
                // I'm the child process -- forward stdout and stderr to parent, call function, and exit, but exit
                // early if parent dies

                drop(alive_tx);

                thread::spawn(move || {
                    let mut buffer = [0u8; 1];

                    unsafe {
                        libc::read(alive_rx.0, buffer.as_mut_ptr() as *mut c_void, buffer.len())
                    };

                    // if the above read returns, we assume the parent is dead, so time to exit
                    process::exit(1);
                });

                // stdout (FD 1)
                if -1 == unsafe { libc::dup2(out_tx.0, 1) } {
                    return Err(anyhow!("dup2 failed; errno: {}", errno()));
                }

                // stderr (FD 2)
                if -1 == unsafe { libc::dup2(err_tx.0, 2) } {
                    return Err(anyhow!("dup2 failed; errno: {}", errno()));
                }

                if let Err(e) = fun() {
                    eprintln!("{:?}", e);
                    process::exit(1);
                } else {
                    process::exit(0);
                }
            }
            pid => {
                // I'm the parent process -- spawn a thread to monitor the child

                Ok(thread::spawn(move || {
                    let _alive_tx = alive_tx;
                    let mut stderr = Vec::<u8>::new();
                    err_rx.forward(&mut stderr)?;

                    let mut status = 0;
                    if -1 == unsafe { libc::waitpid(pid, &mut status, 0) } {
                        return Err(anyhow!("waitpid failed; errno: {}", errno()));
                    }

                    if libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0 {
                        Ok(())
                    } else {
                        Err(anyhow!(
                            "child exited{}{}",
                            if libc::WIFEXITED(status) {
                                format!(" (exit status {})", libc::WEXITSTATUS(status))
                            } else if libc::WIFSIGNALED(status) {
                                format!(" (killed by signal {})", libc::WTERMSIG(status))
                            } else {
                                String::new()
                            },
                            if stderr.is_empty() {
                                String::new()
                            } else {
                                format!("; stderr:\n{}", String::from_utf8_lossy(&stderr))
                            }
                        ))
                    }
                }))
            }
        }
    }
}
