//! Wakes the config watcher when `config.yaml` or the top level of `auth-dir` may have
//! changed, so an idle server does no file work at all. Each platform uses its own
//! kernel facility, without an extra thread except on Windows:
//!
//! - Linux: one inotify descriptor on the config file's folder and the auth folder,
//!   registered with tokio's reactor.
//! - macOS: one kqueue descriptor, registered with tokio's own kqueue, plus one
//!   descriptor per watched file or folder (kqueue reports writes per open file).
//! - Windows: one thread parked in `WaitForMultipleObjects` on `ReadDirectoryChangesW`
//!   for both folders.
//!
//! An event only says "look again": the watcher still compares content hashes and
//! waits for writes to settle. When no facility can be set up (another OS, NFS,
//! descriptor limits), it falls back to looking every 2 seconds and logs that once.
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

const FALLBACK_POLL: Duration = Duration::from_secs(2);

/// What the watcher looks at.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Targets {
    pub config: PathBuf,
    pub auth_dir: PathBuf,
    /// The auth files seen last; only kqueue needs them.
    pub files: Vec<PathBuf>,
}

/// One folder to watch and which of its entries matter.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Dir {
    path: PathBuf,
    /// Entry names that matter (the config file); `None` for any entry.
    names: Option<Vec<std::ffi::OsString>>,
    /// Whether `*.json` entries matter (the auth folder).
    json: bool,
}

impl Dir {
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    fn matters(&self, name: &std::ffi::OsStr) -> bool {
        let lossy = name.to_string_lossy();
        (self.json && lossy.to_lowercase().ends_with(".json"))
            || self.names.as_ref().is_none_or(|names| {
                names.iter().any(|n| {
                    if cfg!(windows) {
                        n.to_string_lossy().eq_ignore_ascii_case(&lossy)
                    } else {
                        n == name
                    }
                })
            })
    }
}

fn parent(path: &Path) -> PathBuf {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_owned(),
        _ => PathBuf::from("."),
    }
}

/// The folders to watch: the config file's folder (and its symlink target's), and the
/// auth folder, or its nearest existing ancestor until it is created.
fn dirs(t: &Targets) -> Vec<Dir> {
    let mut out: Vec<Dir> = Vec::new();
    let mut add = |path: PathBuf, name: Option<std::ffi::OsString>, json: bool| {
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        match out.iter_mut().find(|d| d.path == path) {
            Some(d) => {
                d.json |= json;
                match (&mut d.names, name) {
                    (Some(names), Some(n)) => names.push(n),
                    (names, None) if !json => *names = None,
                    _ => {}
                }
            }
            None => out.push(Dir {
                path,
                names: name.map(|n| vec![n]).or(json.then(Vec::new)),
                json,
            }),
        }
    };
    let name = |p: &Path| p.file_name().map(ToOwned::to_owned);
    if let Some(n) = name(&t.config) {
        add(parent(&t.config), Some(n), false);
    }
    if let Ok(real) = std::fs::canonicalize(&t.config)
        && let Some(n) = name(&real)
    {
        add(parent(&real), Some(n), false);
    }
    if t.auth_dir.is_dir() {
        add(t.auth_dir.clone(), None, true);
    } else if let Some(ancestor) = t.auth_dir.ancestors().skip(1).find(|a| a.is_dir()) {
        // Any change there may be the auth folder appearing.
        add(ancestor.to_owned(), None, false);
    }
    out
}

pub(crate) struct Events {
    watch: Option<sys::Watch>,
}

impl Events {
    pub fn new(targets: &Targets) -> Self {
        let watch = sys::Watch::new(targets).map_err(fall_back).ok();
        Self { watch }
    }

    /// Whether events come from the kernel rather than the 2-second fallback.
    #[cfg(test)]
    pub fn native(&self) -> bool {
        self.watch.is_some()
    }

    /// Returns when something may have changed.
    pub async fn changed(&mut self) {
        match &mut self.watch {
            Some(watch) => {
                if let Err(e) = watch.changed().await {
                    fall_back(e);
                    self.watch = None;
                }
            }
            None => tokio::time::sleep(FALLBACK_POLL).await,
        }
    }

    /// Follows a moved auth folder, a replaced config file and, on macOS, the current
    /// auth files. Called after each applied change.
    pub fn retarget(&mut self, targets: &Targets) {
        if let Some(watch) = &mut self.watch
            && let Err(e) = watch.retarget(targets)
        {
            fall_back(e);
            self.watch = None;
        }
    }
}

fn fall_back(error: io::Error) {
    static LOGGED: std::sync::Once = std::sync::Once::new();
    LOGGED.call_once(|| {
        tracing::warn!(
            "file change notifications unavailable ({error}); checking config and auth files every 2s instead"
        );
    });
}

#[cfg(target_os = "linux")]
mod sys {
    use super::{Dir, Targets, dirs};
    use std::collections::HashMap;
    use std::ffi::{CString, OsStr};
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use tokio::io::unix::AsyncFd;

    const MASK: u32 = libc::IN_CREATE
        | libc::IN_DELETE
        | libc::IN_MODIFY
        | libc::IN_CLOSE_WRITE
        | libc::IN_ATTRIB
        | libc::IN_MOVED_FROM
        | libc::IN_MOVED_TO
        | libc::IN_DELETE_SELF
        | libc::IN_MOVE_SELF;

    pub struct Watch {
        fd: AsyncFd<OwnedFd>,
        dirs: Vec<Dir>,
        watches: HashMap<i32, Vec<Dir>>,
    }

    impl Watch {
        pub fn new(targets: &Targets) -> io::Result<Self> {
            // SAFETY: plain syscall; the descriptor is owned below.
            let raw = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
            if raw < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: `raw` is a fresh descriptor nobody else owns.
            let fd = AsyncFd::new(unsafe { OwnedFd::from_raw_fd(raw) })?;
            let mut watch = Self {
                fd,
                dirs: Vec::new(),
                watches: HashMap::new(),
            };
            watch.retarget(targets)?;
            Ok(watch)
        }

        pub fn retarget(&mut self, targets: &Targets) -> io::Result<()> {
            let next = dirs(targets);
            if next == self.dirs {
                return Ok(());
            }
            for wd in self.watches.keys() {
                // SAFETY: plain syscall on our own descriptor.
                unsafe { libc::inotify_rm_watch(self.fd.as_raw_fd(), *wd) };
            }
            self.watches.clear();
            for dir in &next {
                let path = CString::new(dir.path.as_os_str().as_bytes())?;
                // SAFETY: `path` is a valid C string for the duration of the call.
                let wd =
                    unsafe { libc::inotify_add_watch(self.fd.as_raw_fd(), path.as_ptr(), MASK | libc::IN_ONLYDIR) };
                if wd < 0 {
                    return Err(io::Error::last_os_error());
                }
                self.watches.entry(wd).or_default().push(dir.clone());
            }
            self.dirs = next;
            Ok(())
        }

        pub async fn changed(&mut self) -> io::Result<()> {
            // Large enough for many events with a full NAME_MAX name each.
            let mut buf = [0u8; 16 * 1024];
            loop {
                let mut guard = self.fd.readable().await?;
                let mut relevant = false;
                loop {
                    // SAFETY: reads into `buf`, which outlives the call.
                    let n = unsafe { libc::read(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
                    if n < 0 {
                        let e = io::Error::last_os_error();
                        if e.kind() == io::ErrorKind::WouldBlock {
                            guard.clear_ready();
                            break;
                        }
                        if e.kind() == io::ErrorKind::Interrupted {
                            continue;
                        }
                        return Err(e);
                    }
                    relevant |= self.parse(&buf[..n as usize]);
                }
                if relevant {
                    return Ok(());
                }
            }
        }

        /// Whether any event in `data` concerns a watched name.
        fn parse(&self, mut data: &[u8]) -> bool {
            let header = std::mem::size_of::<libc::inotify_event>();
            let mut relevant = false;
            while data.len() >= header {
                // SAFETY: the kernel writes whole events; read_unaligned copes with
                // the byte buffer's alignment.
                let event = unsafe { std::ptr::read_unaligned(data.as_ptr().cast::<libc::inotify_event>()) };
                let end = (header + event.len as usize).min(data.len());
                let name = &data[header..end];
                let name = OsStr::from_bytes(&name[..name.iter().position(|b| *b == 0).unwrap_or(name.len())]);
                data = &data[end..];
                relevant |= event.mask
                    & (libc::IN_Q_OVERFLOW | libc::IN_IGNORED | libc::IN_DELETE_SELF | libc::IN_MOVE_SELF)
                    != 0
                    || self
                        .watches
                        .get(&event.wd)
                        .is_some_and(|dirs| dirs.iter().any(|d| d.matters(name)));
            }
            relevant
        }
    }
}

#[cfg(target_os = "macos")]
mod sys {
    use super::{Targets, dirs};
    use std::collections::HashMap;
    use std::ffi::CString;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    use std::path::PathBuf;
    use tokio::io::unix::AsyncFd;

    /// macOS `OPEN_MAX`, the most `setrlimit` accepts for descriptors.
    const OPEN_MAX: libc::rlim_t = 10240;

    pub struct Watch {
        kq: AsyncFd<OwnedFd>,
        /// Each watched path with the descriptor and the file identity it was opened at.
        open: HashMap<PathBuf, (OwnedFd, (u64, u64))>,
    }

    fn identity(path: &std::path::Path) -> Option<(u64, u64)> {
        std::fs::metadata(path).ok().map(|m| (m.dev(), m.ino()))
    }

    impl Watch {
        pub fn new(targets: &Targets) -> io::Result<Self> {
            // SAFETY: plain syscall; the descriptor is owned below.
            let raw = unsafe { libc::kqueue() };
            if raw < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: `raw` is a fresh descriptor nobody else owns.
            let owned = unsafe { OwnedFd::from_raw_fd(raw) };
            // SAFETY: plain syscall on our own descriptor.
            unsafe { libc::fcntl(raw, libc::F_SETFD, libc::FD_CLOEXEC) };
            let mut watch = Self {
                kq: AsyncFd::new(owned)?,
                open: HashMap::new(),
            };
            watch.retarget(targets)?;
            Ok(watch)
        }

        pub fn retarget(&mut self, targets: &Targets) -> io::Result<()> {
            let mut want: Vec<PathBuf> = dirs(targets).into_iter().map(|d| d.path).collect();
            want.push(targets.config.clone());
            if let Ok(real) = std::fs::canonicalize(&targets.config) {
                want.push(real);
            }
            want.extend(targets.files.iter().cloned());
            want.sort();
            want.dedup();
            raise_descriptor_limit(want.len());
            // Closing a descriptor drops its kqueue registration.
            self.open
                .retain(|path, (_, id)| want.contains(path) && identity(path) == Some(*id));
            for path in want {
                if self.open.contains_key(&path) {
                    continue;
                }
                let Some(id) = identity(&path) else { continue };
                let c = CString::new(path.as_os_str().as_bytes())?;
                // SAFETY: `c` is a valid C string for the duration of the call.
                let fd = unsafe { libc::open(c.as_ptr(), libc::O_EVTONLY | libc::O_CLOEXEC) };
                if fd < 0 {
                    let e = io::Error::last_os_error();
                    // Removed since it was listed: the next change event covers it.
                    if e.kind() == io::ErrorKind::NotFound {
                        continue;
                    }
                    return Err(e);
                }
                // SAFETY: `fd` is a fresh descriptor nobody else owns.
                let fd = unsafe { OwnedFd::from_raw_fd(fd) };
                // SAFETY: zeroed is a valid kevent; the fields that matter are set below.
                let mut change: libc::kevent = unsafe { std::mem::zeroed() };
                change.ident = fd.as_raw_fd() as libc::uintptr_t;
                change.filter = libc::EVFILT_VNODE;
                change.flags = libc::EV_ADD | libc::EV_CLEAR;
                change.fflags = libc::NOTE_WRITE
                    | libc::NOTE_EXTEND
                    | libc::NOTE_ATTRIB
                    | libc::NOTE_DELETE
                    | libc::NOTE_RENAME
                    | libc::NOTE_REVOKE;
                // SAFETY: one valid change, no event output.
                let rc = unsafe {
                    libc::kevent(
                        self.kq.as_raw_fd(),
                        &change,
                        1,
                        std::ptr::null_mut(),
                        0,
                        std::ptr::null(),
                    )
                };
                if rc < 0 {
                    return Err(io::Error::last_os_error());
                }
                self.open.insert(path, (fd, id));
            }
            Ok(())
        }

        pub async fn changed(&mut self) -> io::Result<()> {
            loop {
                let mut guard = self.kq.readable().await?;
                // SAFETY: zeroed kevents are valid output slots.
                let mut events: [libc::kevent; 32] = unsafe { std::mem::zeroed() };
                let zero = libc::timespec { tv_sec: 0, tv_nsec: 0 };
                let mut any = false;
                loop {
                    // SAFETY: the output array outlives the call; a zero timeout polls.
                    let n = unsafe {
                        libc::kevent(
                            self.kq.as_raw_fd(),
                            std::ptr::null(),
                            0,
                            events.as_mut_ptr(),
                            events.len() as libc::c_int,
                            &zero,
                        )
                    };
                    if n < 0 {
                        let e = io::Error::last_os_error();
                        if e.kind() == io::ErrorKind::Interrupted {
                            continue;
                        }
                        return Err(e);
                    }
                    if n == 0 {
                        guard.clear_ready();
                        break;
                    }
                    any = true;
                }
                // ponytail: every event on a watched descriptor counts, including
                // unrelated entries of the config file's folder; a busy folder costs
                // one hash-checked look per change.
                if any {
                    return Ok(());
                }
            }
        }
    }

    /// One descriptor per auth file can pass the 256-descriptor default soft limit; raise
    /// it toward the hard limit as Go's runtime does at startup.
    fn raise_descriptor_limit(watched: usize) {
        let need = (watched as libc::rlim_t).saturating_add(256);
        // SAFETY: getrlimit/setrlimit read and write only the struct given.
        unsafe {
            let mut lim: libc::rlimit = std::mem::zeroed();
            if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) != 0 || lim.rlim_cur >= need {
                return;
            }
            lim.rlim_cur = lim.rlim_max.min(OPEN_MAX).max(lim.rlim_cur);
            libc::setrlimit(libc::RLIMIT_NOFILE, &lim);
        }
    }
}

#[cfg(windows)]
mod sys {
    use super::{Dir, Targets, dirs};
    use std::ffi::OsString;
    use std::io;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::sync::{Arc, Mutex, PoisonError};
    use tokio::sync::Notify;
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_IO_PENDING, HANDLE, INVALID_HANDLE_VALUE, WAIT_FAILED, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY,
        FILE_NOTIFY_CHANGE_ATTRIBUTES, FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME,
        FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE, FILE_NOTIFY_INFORMATION, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, ReadDirectoryChangesW,
    };
    use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
    use windows_sys::Win32::System::Threading::{CreateEventW, INFINITE, SetEvent, WaitForMultipleObjects};

    const FILTER: u32 = FILE_NOTIFY_CHANGE_FILE_NAME
        | FILE_NOTIFY_CHANGE_DIR_NAME
        | FILE_NOTIFY_CHANGE_ATTRIBUTES
        | FILE_NOTIFY_CHANGE_SIZE
        | FILE_NOTIFY_CHANGE_LAST_WRITE;

    /// An owned event or file handle.
    struct Handle(HANDLE);
    // SAFETY: kernel handles may be used from any thread.
    unsafe impl Send for Handle {}
    unsafe impl Sync for Handle {}
    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: the handle is owned and closed once.
            unsafe { CloseHandle(self.0) };
        }
    }

    fn event() -> io::Result<Handle> {
        // SAFETY: an unnamed auto-reset event with default security.
        let h = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
        if h.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Handle(h))
    }

    struct Shared {
        /// Signalled to make the thread reread `next` (or stop when it is `None`).
        control: Handle,
        next: Mutex<Option<Vec<Dir>>>,
        changed: Notify,
        failed: Mutex<Option<io::Error>>,
    }

    pub struct Watch {
        shared: Arc<Shared>,
        dirs: Vec<Dir>,
    }

    impl Watch {
        pub fn new(targets: &Targets) -> io::Result<Self> {
            let dirs = dirs(targets);
            let shared = Arc::new(Shared {
                control: event()?,
                next: Mutex::new(Some(dirs.clone())),
                changed: Notify::new(),
                failed: Mutex::new(None),
            });
            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            let thread_shared = shared.clone();
            std::thread::Builder::new()
                .name("config-watch".into())
                .spawn(move || run(&thread_shared, ready_tx))?;
            ready_rx.recv().map_err(|_| io::Error::other("watch thread exited"))??;
            Ok(Self { shared, dirs })
        }

        pub fn retarget(&mut self, targets: &Targets) -> io::Result<()> {
            let next = dirs(targets);
            if next == self.dirs {
                return Ok(());
            }
            *self.shared.next.lock().unwrap_or_else(PoisonError::into_inner) = Some(next.clone());
            self.dirs = next;
            // SAFETY: a valid event handle.
            unsafe { SetEvent(self.shared.control.0) };
            Ok(())
        }

        pub async fn changed(&mut self) -> io::Result<()> {
            self.shared.changed.notified().await;
            match self.shared.failed.lock().unwrap_or_else(PoisonError::into_inner).take() {
                Some(e) => Err(e),
                None => Ok(()),
            }
        }
    }

    impl Drop for Watch {
        fn drop(&mut self) {
            *self.shared.next.lock().unwrap_or_else(PoisonError::into_inner) = None;
            // SAFETY: a valid event handle.
            unsafe { SetEvent(self.shared.control.0) };
        }
    }

    /// One folder with its pending overlapped read.
    struct Pending {
        dir: Dir,
        handle: Handle,
        done: Handle,
        overlapped: Box<OVERLAPPED>,
        /// DWORD-aligned, as ReadDirectoryChangesW requires.
        buf: Box<[u32; 16 * 1024]>,
    }

    impl Pending {
        fn open(dir: &Dir) -> io::Result<Self> {
            let wide: Vec<u16> = dir.path.as_os_str().encode_wide().chain([0]).collect();
            // SAFETY: `wide` is NUL-terminated and outlives the call.
            let handle = unsafe {
                CreateFileW(
                    wide.as_ptr(),
                    FILE_LIST_DIRECTORY,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                    std::ptr::null_mut(),
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }
            let handle = Handle(handle);
            let done = event()?;
            // SAFETY: zeroed is a valid OVERLAPPED.
            let mut overlapped: Box<OVERLAPPED> = Box::new(unsafe { std::mem::zeroed() });
            overlapped.hEvent = done.0;
            let mut pending = Self {
                dir: dir.clone(),
                handle,
                done,
                overlapped,
                buf: Box::new([0; 16 * 1024]),
            };
            pending.read()?;
            Ok(pending)
        }

        fn read(&mut self) -> io::Result<()> {
            // SAFETY: the buffer and OVERLAPPED are boxed, so they stay in place until
            // the read completes or is cancelled in Drop.
            let ok = unsafe {
                ReadDirectoryChangesW(
                    self.handle.0,
                    self.buf.as_mut_ptr().cast(),
                    std::mem::size_of_val(&*self.buf) as u32,
                    0,
                    FILTER,
                    std::ptr::null_mut(),
                    &mut *self.overlapped,
                    None,
                )
            };
            if ok == 0 {
                let e = io::Error::last_os_error();
                if e.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
                    return Err(e);
                }
            }
            Ok(())
        }

        /// Whether the completed read names a watched entry; then reads again.
        fn finish(&mut self) -> io::Result<bool> {
            let mut bytes = 0u32;
            // SAFETY: the read completed (its event fired); no wait is requested.
            let ok = unsafe { GetOverlappedResult(self.handle.0, &*self.overlapped, &mut bytes, 0) };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            // Zero bytes: the buffer overflowed and the changes are unknown.
            let mut relevant = bytes == 0;
            let base = self.buf.as_ptr().cast::<u8>();
            let mut offset = 0usize;
            while (offset as u32) < bytes {
                // SAFETY: the kernel wrote DWORD-aligned records inside `bytes`.
                let info = unsafe { &*base.add(offset).cast::<FILE_NOTIFY_INFORMATION>() };
                let len = info.FileNameLength as usize / 2;
                // SAFETY: FileName holds `len` UTF-16 units inside the record.
                let name = unsafe { std::slice::from_raw_parts(info.FileName.as_ptr(), len) };
                relevant |= self.dir.matters(&OsString::from_wide(name));
                if info.NextEntryOffset == 0 {
                    break;
                }
                offset += info.NextEntryOffset as usize;
            }
            self.read()?;
            Ok(relevant)
        }
    }

    impl Drop for Pending {
        fn drop(&mut self) {
            // SAFETY: cancel the outstanding read and wait for it, so the kernel stops
            // writing into the buffer before it is freed.
            unsafe {
                CancelIoEx(self.handle.0, &*self.overlapped);
                let mut bytes = 0u32;
                GetOverlappedResult(self.handle.0, &*self.overlapped, &mut bytes, 1);
            }
        }
    }

    fn run(shared: &Shared, ready: std::sync::mpsc::Sender<io::Result<()>>) {
        let mut ready = Some(ready);
        loop {
            let Some(dirs) = shared.next.lock().unwrap_or_else(PoisonError::into_inner).clone() else {
                return;
            };
            let opened: io::Result<Vec<Pending>> = dirs.iter().map(Pending::open).collect();
            let mut pending = match opened {
                Ok(p) => p,
                Err(e) => {
                    match ready.take() {
                        Some(tx) => drop(tx.send(Err(e))),
                        None => {
                            *shared.failed.lock().unwrap_or_else(PoisonError::into_inner) = Some(e);
                            shared.changed.notify_one();
                        }
                    }
                    return;
                }
            };
            if let Some(tx) = ready.take() {
                let _ = tx.send(Ok(()));
            }
            loop {
                let handles: Vec<HANDLE> = std::iter::once(shared.control.0)
                    .chain(pending.iter().map(|p| p.done.0))
                    .collect();
                // SAFETY: every handle stays open while waited on.
                let r = unsafe { WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), 0, INFINITE) };
                if r == WAIT_FAILED {
                    *shared.failed.lock().unwrap_or_else(PoisonError::into_inner) = Some(io::Error::last_os_error());
                    shared.changed.notify_one();
                    return;
                }
                let index = r.wrapping_sub(WAIT_OBJECT_0) as usize;
                if index == 0 {
                    break; // Retarget or stop.
                }
                match pending.get_mut(index - 1).map(Pending::finish) {
                    Some(Ok(true)) => shared.changed.notify_one(),
                    Some(Ok(false)) | None => {}
                    Some(Err(e)) => {
                        *shared.failed.lock().unwrap_or_else(PoisonError::into_inner) = Some(e);
                        shared.changed.notify_one();
                        return;
                    }
                }
            }
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod sys {
    use super::Targets;
    use std::io;

    pub enum Watch {}

    impl Watch {
        pub fn new(_: &Targets) -> io::Result<Self> {
            Err(io::Error::new(io::ErrorKind::Unsupported, "not implemented on this OS"))
        }
        pub fn retarget(&mut self, _: &Targets) -> io::Result<()> {
            match *self {}
        }
        pub async fn changed(&mut self) -> io::Result<()> {
            match *self {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cpa-fs-events-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("auth")).unwrap();
        dir
    }

    async fn fires(events: &mut Events) -> bool {
        tokio::time::timeout(Duration::from_millis(500), events.changed())
            .await
            .is_ok()
    }

    #[test]
    fn shared_folder_merges_roles() {
        let root = temp();
        let t = Targets {
            config: root.join("auth/config.yaml"),
            auth_dir: root.join("auth"),
            files: Vec::new(),
        };
        let d = dirs(&t);
        assert_eq!(d.len(), 1, "{d:?}");
        assert!(d[0].matters("config.yaml".as_ref()));
        assert!(d[0].matters("a.JSON".as_ref()));
        assert!(!d[0].matters("notes.txt".as_ref()));
        // A missing auth folder is watched through its nearest existing ancestor.
        let t = Targets {
            config: root.join("config.yaml"),
            auth_dir: root.join("later/auth"),
            files: Vec::new(),
        };
        let d = dirs(&t);
        assert_eq!(d.len(), 1, "{d:?}");
        assert!(d[0].matters("anything".as_ref()));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[tokio::test]
    async fn wakes_for_watched_files_only() {
        let root = temp();
        let mut t = Targets {
            config: root.join("config.yaml"),
            auth_dir: root.join("auth"),
            files: Vec::new(),
        };
        std::fs::write(&t.config, "port: 1\n").unwrap();
        std::fs::write(root.join("auth/a.json"), "{}").unwrap();
        t.files.push(root.join("auth/a.json"));
        let mut events = Events::new(&t);
        assert!(events.native());
        assert!(!fires(&mut events).await, "no event while nothing changes");
        // An unrelated file next to the config is ignored (kqueue sees only folders).
        std::fs::write(root.join("notes.txt"), "x").unwrap();
        if cfg!(not(target_os = "macos")) {
            assert!(!fires(&mut events).await, "unrelated file woke the watcher");
        }
        let _ = fires(&mut events).await;
        std::fs::write(&t.config, "port: 2\n").unwrap();
        assert!(fires(&mut events).await, "config edit");
        std::fs::write(root.join("auth/a.json"), r#"{"a":1}"#).unwrap();
        assert!(fires(&mut events).await, "auth file edit in place");
        std::fs::write(root.join("auth/b.json"), "{}").unwrap();
        assert!(fires(&mut events).await, "new auth file");
        // An atomically replaced config is still watched after a retarget.
        let tmp = root.join("config.yaml.tmp");
        std::fs::write(&tmp, "port: 3\n").unwrap();
        std::fs::rename(&tmp, &t.config).unwrap();
        assert!(fires(&mut events).await, "config replaced");
        while fires(&mut events).await {}
        events.retarget(&t);
        std::fs::write(&t.config, "port: 4\n").unwrap();
        assert!(fires(&mut events).await, "edit after replacement");
        // A moved auth folder is followed.
        while fires(&mut events).await {}
        std::fs::create_dir_all(root.join("auth2")).unwrap();
        t.auth_dir = root.join("auth2");
        t.files.clear();
        events.retarget(&t);
        std::fs::write(root.join("auth2/c.json"), "{}").unwrap();
        assert!(fires(&mut events).await, "new auth folder");
        drop(events);
        std::fs::remove_dir_all(root).unwrap();
    }
}
