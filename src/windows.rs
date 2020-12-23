use crate::{bitmask::BitMask, Error, Result};
use memmap::MmapMut;
use std::{
    cell::UnsafeCell,
    convert::TryInto,
    ffi::{CStr, CString},
    ptr, slice,
    sync::{
        atomic::{AtomicU32, Ordering::SeqCst},
        Arc, Mutex,
    },
    time::Duration,
};
use tempfile::NamedTempFile;
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

macro_rules! expect {
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
            ptr::write(self.threads.get(), BitMask::default());
            ptr::write(self.waiters.get(), BitMask::default());
        }
        self.read.store(crate::BEGINNING, SeqCst);
        self.write.store(crate::BEGINNING, SeqCst);

        Ok(())
    }
}

pub struct View {
    buffer: Arc<UnsafeCell<Buffer>>,
    index: u8,
}

impl View {
    pub fn try_new(buffer: Arc<UnsafeCell<Buffer>>) -> Result<Self> {
        let mut lock = unsafe { (*buffer.get()).lock()? };

        let index = lock.threads().zeros().next().ok_or(Error::TooManySenders)?;

        *lock.threads() = lock.threads().set(index);

        Ok(Self { buffer, index })
    }

    pub fn buffer(&self) -> &Buffer {
        unsafe { &*self.buffer.get() }
    }

    #[allow(clippy::mut_from_ref)]
    pub fn map_mut(&self) -> &mut MmapMut {
        unsafe { (*self.buffer.get()).map_mut() }
    }
}

impl Clone for View {
    fn clone(&self) -> Self {
        Self::try_new(self.buffer.clone()).unwrap()
    }
}

impl Drop for View {
    fn drop(&mut self) {
        if let Ok(mut lock) = self.buffer().lock() {
            *lock.threads() = lock.threads().clear(self.index);
        }
    }
}

pub struct Buffer {
    map: MmapMut,
    path: String,
    _file: Option<NamedTempFile>,
    mutex: HANDLE,
    semaphores: Mutex<[HANDLE; BitMask::capacity() as usize]>,
}

impl Buffer {
    pub fn try_new(path: &str, map: MmapMut, file: Option<NamedTempFile>) -> Result<Self> {
        let mut buffer = Self {
            map,
            path: path
                .chars()
                .map(|c| if c.is_alphanumeric() { c } else { '-' })
                .collect(),
            _file: file,
            mutex: ptr::null_mut(),
            semaphores: Mutex::new([ptr::null_mut(); BitMask::capacity() as usize]),
        };

        let mutex_string = format!("{}-mutex", buffer.path);
        let mutex_name = CString::new(mutex_string.clone())
            .expect("should not fail -- null characters were replaced earlier");

        buffer.mutex = unsafe {
            synchapi::CreateMutexA(ptr::null_mut(), minwindef::FALSE, mutex_name.as_ptr())
        };

        if buffer.mutex.is_null() {
            return Err(Error::Runtime(format!(
                "CreateMutex for {} failed: {}",
                mutex_string,
                get_last_error()
            )));
        }

        Ok(buffer)
    }

    fn semaphore(&self, index: u8) -> Result<HANDLE> {
        let index = index as usize;
        let mut semaphores = self.semaphores.lock().unwrap();

        if semaphores[index].is_null() {
            let semaphore_string = format!("{}-semaphore-{}", self.path, index);
            let semaphore_name = CString::new(semaphore_string.clone())
                .expect("should not fail -- null characters were replaced earlier");

            semaphores[index] = unsafe {
                winbase::CreateSemaphoreA(ptr::null_mut(), 0, 1, semaphore_name.as_ptr())
            };

            if semaphores[index].is_null() {
                return Err(Error::Runtime(format!(
                    "CreateSemaphore for {} failed: {}",
                    semaphore_string,
                    get_last_error()
                )));
            }
        }

        Ok(semaphores[index])
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

impl Drop for Buffer {
    fn drop(&mut self) {
        if !self.mutex.is_null() {
            unsafe { handleapi::CloseHandle(self.mutex) };
        }

        for &semaphore in self.semaphores.lock().unwrap().iter() {
            if !semaphore.is_null() {
                unsafe { handleapi::CloseHandle(semaphore) };
            }
        }
    }
}

pub struct Lock<'a> {
    locked: bool,
    buffer: &'a Buffer,
}

impl<'a> Lock<'a> {
    pub fn try_new(buffer: &Buffer) -> Result<Lock> {
        expect!(
            winbase::WAIT_OBJECT_0
                == unsafe { synchapi::WaitForSingleObject(buffer.mutex, winbase::INFINITE) }
        )?;

        Ok(Lock {
            locked: true,
            buffer,
        })
    }

    fn do_wait(&mut self, view: &View, milliseconds: ULONG) -> Result<()> {
        let index = view.index;

        *self.waiters() = self.waiters().set(index);

        expect!(minwindef::TRUE == unsafe { synchapi::ReleaseMutex(self.buffer.mutex) })?;

        self.locked = false;

        let _ = milliseconds;

        expect!(matches!(
            unsafe { synchapi::WaitForSingleObject(self.buffer.semaphore(index)?, 10000) },
            winbase::WAIT_OBJECT_0 | winerror::WAIT_TIMEOUT
        ))?;

        expect!(
            winbase::WAIT_OBJECT_0
                == unsafe { synchapi::WaitForSingleObject(self.buffer.mutex, winbase::INFINITE) }
        )?;

        self.locked = true;

        *self.waiters() = self.waiters().clear(index);

        Ok(())
    }

    pub fn wait(&mut self, view: &View) -> Result<()> {
        self.do_wait(view, winbase::INFINITE)
    }

    pub fn timed_wait(&mut self, view: &View, timeout: Option<Duration>) -> Result<()> {
        if let Some(timeout) = timeout {
            self.do_wait(
                view,
                timeout.as_millis().try_into().map_err(|_| {
                    Error::Runtime("unable to represent timeout in milliseconds as ULONG".into())
                })?,
            )
        } else {
            self.wait(view)
        }
    }

    pub fn notify_all(&mut self) -> Result<()> {
        let waiters = *self.waiters();

        for index in waiters.ones() {
            // We're ignoring the return value below because TRUE means the sempahore was successfully incremented
            // and FALSE (presumably) means it had already been incremented to its maximum, which is also fine.
            //
            // Unfortunately, there's no reliable way to distinguish between different error conditions
            // programatically since the exact code returned by GetLastError is not considered part of the
            // supported Windows API by Microsoft.
            unsafe {
                synchapi::ReleaseSemaphore(self.buffer.semaphore(index)?, 1, ptr::null_mut())
            };
        }

        *self.waiters() = BitMask::default();

        Ok(())
    }

    fn waiters(&mut self) -> &mut BitMask {
        unsafe { &mut *self.buffer.header().waiters.get() }
    }

    fn threads(&mut self) -> &mut BitMask {
        unsafe { &mut *self.buffer.header().threads.get() }
    }
}

impl<'a> Drop for Lock<'a> {
    fn drop(&mut self) {
        if self.locked {
            unsafe { synchapi::ReleaseMutex(self.buffer.mutex) };
        }
    }
}

#[cfg(any(test, feature = "fork"))]
pub mod test {
    use anyhow::Result;
    use std::thread::{self, JoinHandle};

    pub fn fork<F: Send + 'static + FnOnce() -> Result<()>>(
        fun: F,
    ) -> Result<JoinHandle<Result<()>>> {
        Ok(thread::spawn(fun))
    }
}
