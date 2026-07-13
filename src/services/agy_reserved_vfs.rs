//! A one-shot SQLite VFS that exposes an already-reserved clone file.
//!
//! SQLite's public backup API normally accepts a destination pathname, not an
//! existing `File`. Reopening that pathname would reintroduce the swap race
//! that the reservation is meant to prevent. This VFS gives one SQLite
//! connection a duplicate of the reservation and implements only the regular
//! file operations needed by the online backup API. It never resolves or
//! opens a filesystem pathname.

#![allow(unsafe_code)]

use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use rusqlite::ffi;

use super::{clone_file_identity, CloneFileIdentity};

#[repr(C)]
struct ReservedSqliteFile {
    base: ffi::sqlite3_file,
    file: Option<Mutex<File>>,
    state: *mut ReservedVfsState,
    lock_level: c_int,
}

struct ReservedVfsState {
    reserved: File,
    expected_identity: CloneFileIdentity,
    filename: Vec<u8>,
    default_vfs: *mut ffi::sqlite3_vfs,
    opened: AtomicBool,
}

pub(super) struct Registration {
    name: String,
    _name_c: CString,
    filename: String,
    state: Box<ReservedVfsState>,
    vfs: Box<ffi::sqlite3_vfs>,
    registered: bool,
}

fn callback_result(fallback: c_int, callback: impl FnOnce() -> c_int) -> c_int {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(callback)).unwrap_or(fallback)
}

unsafe fn file_context<'a>(file: *mut ffi::sqlite3_file) -> Option<&'a mut ReservedSqliteFile> {
    file.cast::<ReservedSqliteFile>().as_mut()
}

unsafe fn state_from_vfs<'a>(vfs: *mut ffi::sqlite3_vfs) -> Option<&'a ReservedVfsState> {
    let vfs = vfs.as_ref()?;
    (vfs.pAppData as *const ReservedVfsState).as_ref()
}

unsafe extern "C" fn file_close(file: *mut ffi::sqlite3_file) -> c_int {
    callback_result(ffi::SQLITE_IOERR_CLOSE, || {
        let Some(context) = (unsafe { file_context(file) }) else {
            return ffi::SQLITE_IOERR_CLOSE;
        };
        context.base.pMethods = ptr::null();
        context.file.take();
        if let Some(state) = unsafe { context.state.as_ref() } {
            state.opened.store(false, Ordering::Release);
        }
        ffi::SQLITE_OK
    })
}

unsafe extern "C" fn file_read(
    file: *mut ffi::sqlite3_file,
    output: *mut c_void,
    amount: c_int,
    offset: ffi::sqlite3_int64,
) -> c_int {
    callback_result(ffi::SQLITE_IOERR_READ, || {
        let (Ok(amount), Ok(offset)) = (usize::try_from(amount), u64::try_from(offset)) else {
            return ffi::SQLITE_IOERR_READ;
        };
        if amount == 0 {
            return ffi::SQLITE_OK;
        }
        if output.is_null() {
            return ffi::SQLITE_IOERR_READ;
        }
        let Some(context) = (unsafe { file_context(file) }) else {
            return ffi::SQLITE_IOERR_READ;
        };
        let Some(file) = context.file.as_ref() else {
            return ffi::SQLITE_IOERR_READ;
        };
        let mut file = file.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if file.seek(SeekFrom::Start(offset)).is_err() {
            return ffi::SQLITE_IOERR_SEEK;
        }
        let buffer = unsafe { std::slice::from_raw_parts_mut(output.cast::<u8>(), amount) };
        buffer.fill(0);
        let mut read = 0usize;
        while read < amount {
            match file.read(&mut buffer[read..]) {
                Ok(0) => return ffi::SQLITE_IOERR_SHORT_READ,
                Ok(count) => read += count,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return ffi::SQLITE_IOERR_READ,
            }
        }
        ffi::SQLITE_OK
    })
}

unsafe extern "C" fn file_write(
    file: *mut ffi::sqlite3_file,
    input: *const c_void,
    amount: c_int,
    offset: ffi::sqlite3_int64,
) -> c_int {
    callback_result(ffi::SQLITE_IOERR_WRITE, || {
        let (Ok(amount), Ok(offset)) = (usize::try_from(amount), u64::try_from(offset)) else {
            return ffi::SQLITE_IOERR_WRITE;
        };
        if amount == 0 {
            return ffi::SQLITE_OK;
        }
        if input.is_null() {
            return ffi::SQLITE_IOERR_WRITE;
        }
        let Some(context) = (unsafe { file_context(file) }) else {
            return ffi::SQLITE_IOERR_WRITE;
        };
        let Some(file) = context.file.as_ref() else {
            return ffi::SQLITE_IOERR_WRITE;
        };
        let mut file = file.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if file.seek(SeekFrom::Start(offset)).is_err() {
            return ffi::SQLITE_IOERR_SEEK;
        }
        let buffer = unsafe { std::slice::from_raw_parts(input.cast::<u8>(), amount) };
        let mut written = 0usize;
        while written < amount {
            match file.write(&buffer[written..]) {
                Ok(0) => return ffi::SQLITE_IOERR_WRITE,
                Ok(count) => written += count,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return ffi::SQLITE_IOERR_WRITE,
            }
        }
        ffi::SQLITE_OK
    })
}

unsafe extern "C" fn file_truncate(
    file: *mut ffi::sqlite3_file,
    size: ffi::sqlite3_int64,
) -> c_int {
    callback_result(ffi::SQLITE_IOERR_TRUNCATE, || {
        let Ok(size) = u64::try_from(size) else {
            return ffi::SQLITE_IOERR_TRUNCATE;
        };
        let Some(context) = (unsafe { file_context(file) }) else {
            return ffi::SQLITE_IOERR_TRUNCATE;
        };
        let Some(file) = context.file.as_ref() else {
            return ffi::SQLITE_IOERR_TRUNCATE;
        };
        let file = file.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        match file.set_len(size) {
            Ok(()) => ffi::SQLITE_OK,
            Err(_) => ffi::SQLITE_IOERR_TRUNCATE,
        }
    })
}

unsafe extern "C" fn file_sync(file: *mut ffi::sqlite3_file, flags: c_int) -> c_int {
    callback_result(ffi::SQLITE_IOERR_FSYNC, || {
        let Some(context) = (unsafe { file_context(file) }) else {
            return ffi::SQLITE_IOERR_FSYNC;
        };
        let Some(file) = context.file.as_ref() else {
            return ffi::SQLITE_IOERR_FSYNC;
        };
        let file = file.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let result = if flags & ffi::SQLITE_SYNC_DATAONLY != 0 {
            file.sync_data()
        } else {
            file.sync_all()
        };
        match result {
            Ok(()) => ffi::SQLITE_OK,
            Err(_) => ffi::SQLITE_IOERR_FSYNC,
        }
    })
}

unsafe extern "C" fn file_size(
    file: *mut ffi::sqlite3_file,
    output: *mut ffi::sqlite3_int64,
) -> c_int {
    callback_result(ffi::SQLITE_IOERR_FSTAT, || {
        if output.is_null() {
            return ffi::SQLITE_IOERR_FSTAT;
        }
        let Some(context) = (unsafe { file_context(file) }) else {
            return ffi::SQLITE_IOERR_FSTAT;
        };
        let Some(file) = context.file.as_ref() else {
            return ffi::SQLITE_IOERR_FSTAT;
        };
        let file = file.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let Ok(metadata) = file.metadata() else {
            return ffi::SQLITE_IOERR_FSTAT;
        };
        let Ok(size) = i64::try_from(metadata.len()) else {
            return ffi::SQLITE_IOERR_FSTAT;
        };
        unsafe { *output = size };
        ffi::SQLITE_OK
    })
}

unsafe extern "C" fn file_lock(file: *mut ffi::sqlite3_file, level: c_int) -> c_int {
    callback_result(ffi::SQLITE_IOERR_LOCK, || {
        let Some(context) = (unsafe { file_context(file) }) else {
            return ffi::SQLITE_IOERR_LOCK;
        };
        context.lock_level = context.lock_level.max(level);
        ffi::SQLITE_OK
    })
}

unsafe extern "C" fn file_unlock(file: *mut ffi::sqlite3_file, level: c_int) -> c_int {
    callback_result(ffi::SQLITE_IOERR_UNLOCK, || {
        let Some(context) = (unsafe { file_context(file) }) else {
            return ffi::SQLITE_IOERR_UNLOCK;
        };
        context.lock_level = level;
        ffi::SQLITE_OK
    })
}

unsafe extern "C" fn file_check_reserved_lock(
    file: *mut ffi::sqlite3_file,
    output: *mut c_int,
) -> c_int {
    callback_result(ffi::SQLITE_IOERR_CHECKRESERVEDLOCK, || {
        if output.is_null() {
            return ffi::SQLITE_IOERR_CHECKRESERVEDLOCK;
        }
        let Some(context) = (unsafe { file_context(file) }) else {
            return ffi::SQLITE_IOERR_CHECKRESERVEDLOCK;
        };
        unsafe { *output = i32::from(context.lock_level >= ffi::SQLITE_LOCK_RESERVED) };
        ffi::SQLITE_OK
    })
}

unsafe extern "C" fn file_control(
    _file: *mut ffi::sqlite3_file,
    _operation: c_int,
    _argument: *mut c_void,
) -> c_int {
    ffi::SQLITE_NOTFOUND
}

unsafe extern "C" fn file_sector_size(_file: *mut ffi::sqlite3_file) -> c_int {
    4096
}

unsafe extern "C" fn file_device_characteristics(_file: *mut ffi::sqlite3_file) -> c_int {
    0
}

static IO_METHODS: ffi::sqlite3_io_methods = ffi::sqlite3_io_methods {
    iVersion: 1,
    xClose: Some(file_close),
    xRead: Some(file_read),
    xWrite: Some(file_write),
    xTruncate: Some(file_truncate),
    xSync: Some(file_sync),
    xFileSize: Some(file_size),
    xLock: Some(file_lock),
    xUnlock: Some(file_unlock),
    xCheckReservedLock: Some(file_check_reserved_lock),
    xFileControl: Some(file_control),
    xSectorSize: Some(file_sector_size),
    xDeviceCharacteristics: Some(file_device_characteristics),
    xShmMap: None,
    xShmLock: None,
    xShmBarrier: None,
    xShmUnmap: None,
    xFetch: None,
    xUnfetch: None,
};

unsafe extern "C" fn vfs_open(
    vfs: *mut ffi::sqlite3_vfs,
    name: ffi::sqlite3_filename,
    output: *mut ffi::sqlite3_file,
    flags: c_int,
    output_flags: *mut c_int,
) -> c_int {
    callback_result(ffi::SQLITE_CANTOPEN, || {
        if output.is_null() {
            return ffi::SQLITE_CANTOPEN;
        }
        // SQLite requires pMethods to be NULL on every failed xOpen call,
        // including failures before this VFS constructs its Rust context.
        unsafe { (*output).pMethods = ptr::null() };
        if name.is_null() {
            return ffi::SQLITE_CANTOPEN;
        }
        let Some(state) = (unsafe { state_from_vfs(vfs) }) else {
            return ffi::SQLITE_CANTOPEN;
        };
        if flags & ffi::SQLITE_OPEN_MAIN_DB == 0
            || flags & ffi::SQLITE_OPEN_READWRITE == 0
            || unsafe { CStr::from_ptr(name) }.to_bytes() != state.filename
        {
            return ffi::SQLITE_CANTOPEN;
        }
        if state
            .opened
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return ffi::SQLITE_BUSY;
        }

        let reserved = match state.reserved.try_clone() {
            Ok(file) => file,
            Err(_) => {
                state.opened.store(false, Ordering::Release);
                return ffi::SQLITE_CANTOPEN;
            }
        };
        if clone_file_identity(&reserved).ok() != Some(state.expected_identity) {
            state.opened.store(false, Ordering::Release);
            return ffi::SQLITE_CANTOPEN;
        }

        unsafe {
            ptr::write(
                output.cast::<ReservedSqliteFile>(),
                ReservedSqliteFile {
                    base: ffi::sqlite3_file {
                        pMethods: &IO_METHODS,
                    },
                    file: Some(Mutex::new(reserved)),
                    state: (state as *const ReservedVfsState).cast_mut(),
                    lock_level: ffi::SQLITE_LOCK_NONE,
                },
            );
            if !output_flags.is_null() {
                *output_flags = flags;
            }
        }
        ffi::SQLITE_OK
    })
}

unsafe extern "C" fn vfs_delete(
    vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    _sync_directory: c_int,
) -> c_int {
    callback_result(ffi::SQLITE_IOERR_DELETE, || {
        if name.is_null() {
            return ffi::SQLITE_IOERR_DELETE;
        }
        let Some(state) = (unsafe { state_from_vfs(vfs) }) else {
            return ffi::SQLITE_IOERR_DELETE;
        };
        if unsafe { CStr::from_ptr(name) }.to_bytes() == state.filename {
            ffi::SQLITE_IOERR_DELETE
        } else {
            // Journal and WAL names never exist in this pathname-free VFS.
            ffi::SQLITE_OK
        }
    })
}

unsafe extern "C" fn vfs_access(
    vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    _flags: c_int,
    output: *mut c_int,
) -> c_int {
    callback_result(ffi::SQLITE_IOERR_ACCESS, || {
        if name.is_null() || output.is_null() {
            return ffi::SQLITE_IOERR_ACCESS;
        }
        let Some(state) = (unsafe { state_from_vfs(vfs) }) else {
            return ffi::SQLITE_IOERR_ACCESS;
        };
        unsafe {
            *output = i32::from(CStr::from_ptr(name).to_bytes() == state.filename);
        }
        ffi::SQLITE_OK
    })
}

unsafe extern "C" fn vfs_full_pathname(
    _vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    output_size: c_int,
    output: *mut c_char,
) -> c_int {
    callback_result(ffi::SQLITE_CANTOPEN_FULLPATH, || {
        let Ok(output_size) = usize::try_from(output_size) else {
            return ffi::SQLITE_CANTOPEN_FULLPATH;
        };
        if name.is_null() || output.is_null() {
            return ffi::SQLITE_CANTOPEN_FULLPATH;
        }
        let name = unsafe { CStr::from_ptr(name) }.to_bytes_with_nul();
        if name.len() > output_size {
            return ffi::SQLITE_CANTOPEN_FULLPATH;
        }
        unsafe {
            ptr::copy_nonoverlapping(name.as_ptr(), output.cast::<u8>(), name.len());
        }
        ffi::SQLITE_OK
    })
}

unsafe extern "C" fn vfs_randomness(
    vfs: *mut ffi::sqlite3_vfs,
    amount: c_int,
    output: *mut c_char,
) -> c_int {
    callback_result(0, || {
        let Some(state) = (unsafe { state_from_vfs(vfs) }) else {
            return 0;
        };
        let Some(default) = (unsafe { state.default_vfs.as_ref() }) else {
            return 0;
        };
        match default.xRandomness {
            Some(callback) => unsafe { callback(state.default_vfs, amount, output) },
            None => 0,
        }
    })
}

unsafe extern "C" fn vfs_sleep(vfs: *mut ffi::sqlite3_vfs, micros: c_int) -> c_int {
    callback_result(0, || {
        let Some(state) = (unsafe { state_from_vfs(vfs) }) else {
            return 0;
        };
        let Some(default) = (unsafe { state.default_vfs.as_ref() }) else {
            return 0;
        };
        match default.xSleep {
            Some(callback) => unsafe { callback(state.default_vfs, micros) },
            None => 0,
        }
    })
}

unsafe extern "C" fn vfs_current_time(vfs: *mut ffi::sqlite3_vfs, output: *mut f64) -> c_int {
    callback_result(ffi::SQLITE_IOERR, || {
        let Some(state) = (unsafe { state_from_vfs(vfs) }) else {
            return ffi::SQLITE_IOERR;
        };
        let Some(default) = (unsafe { state.default_vfs.as_ref() }) else {
            return ffi::SQLITE_IOERR;
        };
        match default.xCurrentTime {
            Some(callback) => unsafe { callback(state.default_vfs, output) },
            None => ffi::SQLITE_IOERR,
        }
    })
}

unsafe extern "C" fn vfs_last_error(
    vfs: *mut ffi::sqlite3_vfs,
    size: c_int,
    output: *mut c_char,
) -> c_int {
    callback_result(0, || {
        let Some(state) = (unsafe { state_from_vfs(vfs) }) else {
            return 0;
        };
        let Some(default) = (unsafe { state.default_vfs.as_ref() }) else {
            return 0;
        };
        match default.xGetLastError {
            Some(callback) => unsafe { callback(state.default_vfs, size, output) },
            None => 0,
        }
    })
}

impl Registration {
    pub(super) fn register(
        reserved: &File,
        expected_identity: CloneFileIdentity,
    ) -> Result<Self, String> {
        let held = reserved
            .try_clone()
            .map_err(|error| format!("failed to duplicate reserved SQLite file: {error}"))?;
        if clone_file_identity(&held)
            .map_err(|error| format!("failed to identify reserved SQLite file: {error}"))?
            != expected_identity
        {
            return Err("reserved SQLite file identity changed before VFS registration".into());
        }

        let suffix = format!("{}-{:032x}", std::process::id(), rand::random::<u128>());
        let name = format!("cokacdir-reserved-{suffix}");
        let filename = format!("cokacdir-reserved-database-{suffix}");
        let name_c = CString::new(name.as_bytes())
            .map_err(|_| "generated SQLite VFS name contains NUL".to_string())?;
        let default_vfs = unsafe { ffi::sqlite3_vfs_find(ptr::null()) };
        if default_vfs.is_null() {
            return Err("SQLite default VFS is unavailable".into());
        }

        let mut state = Box::new(ReservedVfsState {
            reserved: held,
            expected_identity,
            filename: filename.as_bytes().to_vec(),
            default_vfs,
            opened: AtomicBool::new(false),
        });
        let mut vfs = Box::new(ffi::sqlite3_vfs {
            iVersion: 1,
            szOsFile: i32::try_from(std::mem::size_of::<ReservedSqliteFile>())
                .map_err(|_| "reserved SQLite file context is too large".to_string())?,
            mxPathname: 1024,
            pNext: ptr::null_mut(),
            zName: name_c.as_ptr(),
            pAppData: (&mut *state as *mut ReservedVfsState).cast(),
            xOpen: Some(vfs_open),
            xDelete: Some(vfs_delete),
            xAccess: Some(vfs_access),
            xFullPathname: Some(vfs_full_pathname),
            xDlOpen: None,
            xDlError: None,
            xDlSym: None,
            xDlClose: None,
            xRandomness: Some(vfs_randomness),
            xSleep: Some(vfs_sleep),
            xCurrentTime: Some(vfs_current_time),
            xGetLastError: Some(vfs_last_error),
            xCurrentTimeInt64: None,
            xSetSystemCall: None,
            xGetSystemCall: None,
            xNextSystemCall: None,
        });
        let result = unsafe { ffi::sqlite3_vfs_register(&mut *vfs, 0) };
        if result != ffi::SQLITE_OK {
            return Err(format!(
                "failed to register reserved SQLite VFS: code {result}"
            ));
        }

        Ok(Self {
            name,
            _name_c: name_c,
            filename,
            state,
            vfs,
            registered: true,
        })
    }

    pub(super) fn name(&self) -> &str {
        &self.name
    }

    pub(super) fn filename(&self) -> &str {
        &self.filename
    }

    pub(super) fn unregister(mut self) -> Result<(), String> {
        if self.state.opened.load(Ordering::Acquire) {
            return Err("reserved SQLite VFS still has an open database handle".into());
        }
        let result = unsafe { ffi::sqlite3_vfs_unregister(&mut *self.vfs) };
        if result != ffi::SQLITE_OK {
            return Err(format!(
                "failed to unregister reserved SQLite VFS: code {result}"
            ));
        }
        self.registered = false;
        Ok(())
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        if self.registered {
            let _ = unsafe { ffi::sqlite3_vfs_unregister(&mut *self.vfs) };
            self.registered = false;
        }
    }
}
