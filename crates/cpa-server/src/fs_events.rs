//! Wakes the config watcher when `config.yaml` or the top level of `auth-dir` may have
//! changed, so an idle server does no file work at all. Each platform uses its own
//! kernel facility, without an extra thread except on Windows:
//!
//! - Linux: one inotify descriptor on the config file's folder, the config file itself
//!   (so an in-place edit through a bind mount of the single file is seen) and the auth
//!   folder, registered with tokio's reactor.
//! - macOS: one kqueue descriptor, registered with tokio's own kqueue, plus one
//!   descriptor per watched file or folder (kqueue reports writes per open file).
//! - Windows: one thread parked in `WaitForMultipleObjects` on `ReadDirectoryChangesW`
//!   for both folders, and for their parents (folder renames only).
//!
//! An event only says "look again": the watcher still compares content hashes and
//! waits for writes to settle. After each wake the caller retargets ([`Events::retarget`])
//! before it looks, so replaced, moved or newly created folders are followed. When no
//! facility can be set up (inotify or descriptor limits, or an operating system other
//! than Linux, macOS and Windows), it falls back to looking every 2 seconds and logs
//! that once. As in Go, a change made by another machine on a network or FUSE file
//! system (NFS, SMB, 9p such as WSL2's /mnt/c, sshfs) raises no event here and is seen
//! with the next local change.
use std::ffi::OsString;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

const FALLBACK_POLL: Duration = Duration::from_secs(2);

/// What the watcher looks at.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Targets {
    pub config: PathBuf,
    pub auth_dir: PathBuf,
    /// The auth files seen last: kqueue watches each one, and symlinked ones have their
    /// links followed on every platform.
    pub files: Vec<PathBuf>,
}

/// One folder (or, on Linux, the config file) to watch and which of its entries matter.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Dir {
    path: PathBuf,
    /// Entry names that matter; `None` for any (a watched file's own events).
    names: Option<Vec<OsString>>,
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

/// `""` (the parent of a relative single-component path) is the working directory.
fn dot(path: &Path) -> &Path {
    if path.as_os_str().is_empty() {
        Path::new(".")
    } else {
        path
    }
}

type Add<'a> = dyn FnMut(PathBuf, Option<OsString>, bool) + 'a;

/// Watches the nearest existing ancestor of the missing folder `missing`, for the first
/// missing component only, so a busy ancestor such as the home folder wakes nothing
/// else. Each retarget moves the watch one level down as `mkdir -p` proceeds.
fn add_missing(missing: &Path, add: &mut Add<'_>) {
    if let Some(ancestor) = missing.ancestors().skip(1).find(|a| dot(a).is_dir()) {
        let first = missing
            .strip_prefix(ancestor)
            .ok()
            .and_then(|rest| rest.components().next())
            .map(|c| c.as_os_str().to_owned());
        add(dot(ancestor).to_owned(), first, false);
    }
}

/// For each symlink on the way to `path`, watches the folder holding the link for the
/// link's name: a Kubernetes `..data` swap or an `ln -sfn` then wakes the watcher.
/// Cost: one lstat per path component, only when retargeting.
fn follow_links(path: &Path, add: &mut Add<'_>) {
    let mut cur = PathBuf::new();
    let mut rest = path.to_owned();
    let mut hops = 0;
    loop {
        let mut components = rest.components();
        let Some(component) = components.next() else { return };
        let remaining = components.as_path().to_owned();
        match component {
            Component::Prefix(_) | Component::RootDir => cur.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !cur.pop() {
                    cur.push("..");
                }
            }
            Component::Normal(name) => {
                let next = cur.join(name);
                let link = next.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink());
                if link && hops < 40 {
                    hops += 1;
                    add(dot(&cur).to_owned(), Some(name.to_owned()), false);
                    let Ok(target) = std::fs::read_link(&next) else { return };
                    // An absolute target replaces `cur` when pushed.
                    rest = target.join(&remaining);
                    continue;
                }
                cur = next;
            }
        }
        rest = remaining;
    }
}

/// The folders to watch: the config file's folder (and its symlink target's), the auth
/// folder (or its nearest existing ancestor until it is created), and the folders that
/// hold any symlink on the way to them.
fn dirs(t: &Targets) -> Vec<Dir> {
    let mut out: Vec<Dir> = Vec::new();
    let mut add = |path: PathBuf, name: Option<OsString>, json: bool| {
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        match out.iter_mut().find(|d| d.path == path) {
            Some(d) => {
                d.json |= json;
                if let (Some(names), Some(n)) = (&mut d.names, name)
                    && !names.contains(&n)
                {
                    names.push(n);
                }
            }
            None => out.push(Dir {
                path,
                names: Some(name.into_iter().collect()),
                json,
            }),
        }
    };
    let name = |p: &Path| p.file_name().map(ToOwned::to_owned);
    let folder = parent(&t.config);
    match name(&t.config) {
        Some(n) if folder.is_dir() => add(folder, Some(n), false),
        _ => add_missing(&folder, &mut add),
    }
    follow_links(&t.config, &mut add);
    if let Ok(real) = std::fs::canonicalize(&t.config)
        && let Some(n) = name(&real)
    {
        add(parent(&real), Some(n), false);
    }
    if t.auth_dir.is_dir() {
        add(t.auth_dir.clone(), None, true);
    } else {
        add_missing(&t.auth_dir, &mut add);
    }
    follow_links(&t.auth_dir, &mut add);
    for file in &t.files {
        if file.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink()) {
            follow_links(file, &mut add);
            if let Ok(real) = std::fs::canonicalize(file)
                && let Some(n) = name(&real)
            {
                add(parent(&real), Some(n), false);
            }
        }
    }
    out
}

/// File change notifications, or a 2-second poll where they cannot be set up.
#[derive(Default)]
pub(crate) struct Events {
    watch: Option<sys::Watch>,
    /// A retarget added watches: look once more, since a change made before they existed
    /// raised no event.
    rearm: bool,
}

impl Events {
    /// Blocking: on Windows it waits for the watch thread to open the folders.
    pub fn new(targets: &Targets) -> Self {
        let watch = sys::Watch::new(targets).map_err(fall_back).ok();
        Self { watch, rearm: false }
    }

    /// Returns when something may have changed.
    pub async fn changed(&mut self) {
        if std::mem::take(&mut self.rearm) {
            return;
        }
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

    /// Follows moved, replaced or newly created folders, a replaced config file and, on
    /// macOS, the current auth files. Call after every wake, before looking, and after
    /// each applied change. Blocking (file metadata, and on Windows the watch thread).
    pub fn retarget(&mut self, targets: &Targets) {
        if let Some(watch) = &mut self.watch {
            match watch.retarget(targets) {
                Ok(changed) => self.rearm |= changed,
                Err(e) => {
                    fall_back(e);
                    self.watch = None;
                }
            }
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
    use std::path::Path;
    use tokio::io::Interest;
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
    /// The config file's own inode: an in-place write through another path (a bind
    /// mount of the single file) notifies only the file, not this folder.
    const FILE_MASK: u32 =
        libc::IN_MODIFY | libc::IN_CLOSE_WRITE | libc::IN_ATTRIB | libc::IN_DELETE_SELF | libc::IN_MOVE_SELF;

    pub struct Watch {
        fd: AsyncFd<OwnedFd>,
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
            let fd = AsyncFd::with_interest(unsafe { OwnedFd::from_raw_fd(raw) }, Interest::READABLE)?;
            let mut watch = Self {
                fd,
                watches: HashMap::new(),
            };
            watch.retarget(targets)?;
            Ok(watch)
        }

        fn add(&self, path: &Path, mask: u32) -> io::Result<i32> {
            let path = CString::new(path.as_os_str().as_bytes())?;
            // SAFETY: `path` is a valid C string for the duration of the call.
            let wd = unsafe { libc::inotify_add_watch(self.fd.as_raw_fd(), path.as_ptr(), mask) };
            if wd < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(wd)
            }
        }

        /// Adds every watch again: the kernel returns the existing descriptor for an inode
        /// it already watches (no event, so idle stays quiet) and a new one for a folder
        /// or file that was replaced. Descriptors no longer returned are removed. Returns
        /// whether the set changed.
        pub fn retarget(&mut self, targets: &Targets) -> io::Result<bool> {
            let mut next: HashMap<i32, Vec<Dir>> = HashMap::new();
            let mut vanished = false;
            for dir in dirs(targets) {
                match self.add(&dir.path, MASK | libc::IN_ONLYDIR) {
                    Ok(wd) => next.entry(wd).or_default().push(dir),
                    // Removed since `dirs` looked: the next wake retargets again.
                    Err(e) if e.raw_os_error() == Some(libc::ENOENT) => vanished = true,
                    Err(e) => return Err(e),
                }
            }
            match self.add(&targets.config, FILE_MASK) {
                Ok(wd) => next.entry(wd).or_default().push(Dir {
                    path: targets.config.clone(),
                    names: None,
                    json: false,
                }),
                // No file to watch; its folder sees it appear.
                Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR | libc::EACCES)) => {}
                Err(e) => return Err(e),
            }
            for wd in self.watches.keys().filter(|wd| !next.contains_key(wd)) {
                // SAFETY: plain syscall on our own descriptor.
                unsafe { libc::inotify_rm_watch(self.fd.as_raw_fd(), *wd) };
            }
            let changed =
                vanished || next.len() != self.watches.len() || next.keys().any(|wd| !self.watches.contains_key(wd));
            self.watches = next;
            Ok(changed)
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
    use tokio::io::Interest;
    use tokio::io::unix::AsyncFd;

    /// macOS `OPEN_MAX`, used when `kern.maxfilesperproc` cannot be read.
    const OPEN_MAX: libc::rlim_t = 10240;

    pub struct Watch {
        kq: AsyncFd<OwnedFd>,
        /// Each watched path with the descriptor and the file identity it was opened at.
        open: HashMap<PathBuf, (OwnedFd, (u64, u64))>,
    }

    fn identity(path: &std::path::Path) -> Option<(u64, u64)> {
        std::fs::metadata(path).ok().map(|m| (m.dev(), m.ino()))
    }

    /// `kern.maxfilesperproc`, the most descriptors one process may hold.
    fn max_files_per_proc() -> libc::rlim_t {
        let mut value: libc::c_int = 0;
        let mut size = std::mem::size_of::<libc::c_int>();
        let name = c"kern.maxfilesperproc";
        // SAFETY: `value` and `size` describe a c_int output buffer; no new value is set.
        let rc = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                (&raw mut value).cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc == 0 && value > 0 {
            value as libc::rlim_t
        } else {
            OPEN_MAX
        }
    }

    fn soft_limit() -> libc::rlim_t {
        // SAFETY: getrlimit writes only the struct given.
        unsafe {
            let mut lim: libc::rlimit = std::mem::zeroed();
            if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) != 0 {
                return 256;
            }
            lim.rlim_cur
        }
    }

    /// One descriptor per auth file can pass the 256-descriptor default soft limit; raise
    /// it once to the most the system allows, as Go's runtime does at startup. Never
    /// lowers it.
    fn raise_descriptor_limit() {
        // SAFETY: getrlimit/setrlimit read and write only the struct given.
        unsafe {
            let mut lim: libc::rlimit = std::mem::zeroed();
            if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) != 0 {
                return;
            }
            let want = lim.rlim_max.min(max_files_per_proc());
            if want > lim.rlim_cur {
                lim.rlim_cur = want;
                libc::setrlimit(libc::RLIMIT_NOFILE, &lim);
            }
        }
    }

    impl Watch {
        pub fn new(targets: &Targets) -> io::Result<Self> {
            raise_descriptor_limit();
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
                // Read interest only: a kqueue descriptor supports EVFILT_READ, and
                // registering it for writes fails.
                kq: AsyncFd::with_interest(owned, Interest::READABLE)?,
                open: HashMap::new(),
            };
            watch.retarget(targets)?;
            Ok(watch)
        }

        /// Opens what is not watched yet and reopens paths whose file was replaced
        /// (another device and inode). Returns whether anything new is watched.
        pub fn retarget(&mut self, targets: &Targets) -> io::Result<bool> {
            let mut want: Vec<PathBuf> = dirs(targets).into_iter().map(|d| d.path).collect();
            want.push(targets.config.clone());
            if let Ok(real) = std::fs::canonicalize(&targets.config) {
                want.push(real);
            }
            want.extend(targets.files.iter().cloned());
            want.sort();
            want.dedup();
            // Leave at least half the descriptors to sockets and logs; past that, the
            // 2-second poll is the better trade.
            let limit = soft_limit().min(max_files_per_proc());
            if want.len() as libc::rlim_t > limit / 2 {
                return Err(io::Error::from_raw_os_error(libc::EMFILE));
            }
            // Closing a descriptor drops its kqueue registration.
            self.open
                .retain(|path, (_, id)| want.binary_search(path).is_ok() && identity(path) == Some(*id));
            let mut changed = false;
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
                changed = true;
            }
            Ok(changed)
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
}

#[cfg(windows)]
mod sys {
    use super::{Dir, Targets, dirs};
    use std::ffi::{OsStr, OsString};
    use std::io;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::sync::{Arc, Mutex, PoisonError};
    use tokio::sync::Notify;
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_ACCESS_DENIED, ERROR_DELETE_PENDING, ERROR_IO_PENDING, HANDLE, INVALID_HANDLE_VALUE,
        WAIT_FAILED, WAIT_OBJECT_0,
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
    /// A watched folder's parent: only folder creation, deletion and renames, so a busy
    /// parent such as the home folder does not wake the thread for ordinary writes.
    const PARENT_FILTER: u32 = FILE_NOTIFY_CHANGE_DIR_NAME;

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

    /// Windows may report a change under a file's 8.3 alias (`CONFIG~1.YAM`), which no
    /// watched name matches; such a name costs one extra look. Go drops these events.
    pub(super) fn maybe_short_alias(name: &OsStr) -> bool {
        let s = name.to_string_lossy();
        s.len() <= 12 && s.contains('~')
    }

    struct Shared {
        /// Signalled to make the thread reread `next` (or stop when it is `None`).
        control: Handle,
        next: Mutex<Option<Vec<Dir>>>,
        /// Where the thread reports whether it opened the folders of `next`; the caller
        /// waits for it, so no change made after a retarget can be missed.
        opened: Mutex<Ack>,
        changed: Notify,
        failed: Mutex<Option<io::Error>>,
    }

    #[derive(Default)]
    struct Ack {
        reply: Option<std::sync::mpsc::Sender<io::Result<()>>>,
        /// The thread has exited; nobody will answer.
        closed: bool,
    }

    pub struct Watch {
        shared: Arc<Shared>,
        dirs: Vec<Dir>,
    }

    impl Watch {
        pub fn new(targets: &Targets) -> io::Result<Self> {
            let dirs = dirs(targets);
            let (opened_tx, opened_rx) = std::sync::mpsc::channel();
            let shared = Arc::new(Shared {
                control: event()?,
                next: Mutex::new(Some(dirs.clone())),
                opened: Mutex::new(Ack {
                    reply: Some(opened_tx),
                    closed: false,
                }),
                changed: Notify::new(),
                failed: Mutex::new(None),
            });
            let thread_shared = shared.clone();
            std::thread::Builder::new().name("config-watch".into()).spawn(move || {
                run(&thread_shared);
                // Drops a pending reply sender, so a waiting retarget gets an error.
                *thread_shared.opened.lock().unwrap_or_else(PoisonError::into_inner) = Ack {
                    reply: None,
                    closed: true,
                };
            })?;
            opened_rx
                .recv()
                .map_err(|_| io::Error::other("watch thread exited"))??;
            Ok(Self { shared, dirs })
        }

        /// A folder replaced in place is followed through its parent's watch, so only a
        /// changed folder list needs the thread. It signals its own extra look after
        /// reopening, so this returns `false`.
        pub fn retarget(&mut self, targets: &Targets) -> io::Result<bool> {
            let next = dirs(targets);
            if next == self.dirs {
                return Ok(false);
            }
            let (opened_tx, opened_rx) = std::sync::mpsc::channel();
            {
                let mut ack = self.shared.opened.lock().unwrap_or_else(PoisonError::into_inner);
                if ack.closed {
                    return Err(io::Error::other("watch thread exited"));
                }
                ack.reply = Some(opened_tx);
            }
            *self.shared.next.lock().unwrap_or_else(PoisonError::into_inner) = Some(next.clone());
            self.dirs = next;
            // SAFETY: a valid event handle.
            unsafe { SetEvent(self.shared.control.0) };
            // Milliseconds: the thread only reopens the folder handles.
            opened_rx
                .recv()
                .map_err(|_| io::Error::other("watch thread exited"))??;
            Ok(false)
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
        filter: u32,
        /// A watched folder's parent: a match means reopen everything by path.
        parent: bool,
        handle: Handle,
        done: Handle,
        overlapped: Box<OVERLAPPED>,
        /// DWORD-aligned, as ReadDirectoryChangesW requires; the length is in u32 units
        /// (4 KiB). An overflow returns 0 bytes, which means "look again", so a larger
        /// buffer buys nothing.
        buf: Box<[u32; 1024]>,
    }

    impl Pending {
        fn open(dir: &Dir, filter: u32, parent: bool) -> io::Result<Self> {
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
                filter,
                parent,
                handle,
                done,
                overlapped,
                buf: Box::new([0; 1024]),
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
                    self.filter,
                    std::ptr::null_mut(),
                    &mut *self.overlapped,
                    None,
                )
            };
            if ok == 0 {
                let e = io::Error::last_os_error();
                if e.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
                    // The call set Internal to STATUS_PENDING and nothing will complete
                    // it; clear it so Drop's wait returns at once.
                    self.overlapped.Internal = 0;
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
            let name_at = std::mem::offset_of!(FILE_NOTIFY_INFORMATION, FileName);
            while (offset as u32) < bytes && offset + name_at <= bytes as usize {
                // SAFETY: the record header lies inside the `bytes` the kernel wrote.
                // Unaligned reads: records are documented as DWORD-aligned, but nothing
                // here depends on it (Wine, for one, does not align them).
                let info = unsafe { std::ptr::read_unaligned(base.add(offset).cast::<FILE_NOTIFY_INFORMATION>()) };
                let units =
                    (info.FileNameLength as usize / 2).min((bytes as usize).saturating_sub(offset + name_at) / 2);
                let at = unsafe { base.add(offset + name_at).cast::<u16>() };
                // SAFETY: `units` UTF-16 units fit inside `bytes` after the header.
                let name: Vec<u16> = (0..units).map(|i| unsafe { at.add(i).read_unaligned() }).collect();
                let name = OsString::from_wide(&name);
                relevant |= self.dir.matters(&name) || maybe_short_alias(&name);
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

    /// The watched folders, then their parents for folder renames. A parent that cannot
    /// be opened only loses that safety net.
    fn open_all(dirs: &[Dir]) -> io::Result<Vec<Pending>> {
        let mut pending = dirs
            .iter()
            .map(|d| Pending::open(d, FILTER, false))
            .collect::<io::Result<Vec<_>>>()?;
        let mut parents: Vec<Dir> = Vec::new();
        for dir in dirs {
            let (Some(parent), Some(name)) = (dir.path.parent(), dir.path.file_name()) else {
                continue;
            };
            match parents.iter_mut().find(|p| p.path == parent) {
                Some(p) => p.names.get_or_insert_with(Vec::new).push(name.to_owned()),
                None => parents.push(Dir {
                    path: parent.to_owned(),
                    names: Some(vec![name.to_owned()]),
                    json: false,
                }),
            }
        }
        pending.extend(
            parents
                .iter()
                .filter_map(|p| Pending::open(p, PARENT_FILTER, true).ok()),
        );
        Ok(pending)
    }

    fn fail(shared: &Shared, e: io::Error) {
        *shared.failed.lock().unwrap_or_else(PoisonError::into_inner) = Some(e);
        shared.changed.notify_one();
    }

    fn run(shared: &Shared) {
        let mut first = true;
        loop {
            let Some(dirs) = shared.next.lock().unwrap_or_else(PoisonError::into_inner).clone() else {
                return;
            };
            let opened = open_all(&dirs);
            let reply = shared
                .opened
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .reply
                .take();
            let mut pending = match opened {
                Ok(p) => p,
                Err(e) => {
                    match reply {
                        Some(tx) => drop(tx.send(Err(e))),
                        None => fail(shared, e),
                    }
                    return;
                }
            };
            if let Some(tx) = reply {
                let _ = tx.send(Ok(()));
            }
            // A change made while the folders were reopened raised no event: look once.
            if !std::mem::take(&mut first) {
                shared.changed.notify_one();
            }
            loop {
                let handles: Vec<HANDLE> = std::iter::once(shared.control.0)
                    .chain(pending.iter().map(|p| p.done.0))
                    .collect();
                // SAFETY: every handle stays open while waited on.
                let r = unsafe { WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), 0, INFINITE) };
                if r == WAIT_FAILED {
                    return fail(shared, io::Error::last_os_error());
                }
                let index = r.wrapping_sub(WAIT_OBJECT_0) as usize;
                if index == 0 {
                    break; // Retarget or stop.
                }
                let Some(entry) = pending.get_mut(index - 1) else {
                    continue;
                };
                match entry.finish() {
                    // A folder was replaced or renamed: reopen everything by path.
                    Ok(true) if entry.parent => break,
                    Ok(true) => shared.changed.notify_one(),
                    Ok(false) => {}
                    // The folder itself was deleted: stop watching it and look; the
                    // caller's retarget then watches the nearest existing ancestor.
                    Err(e)
                        if matches!(e.raw_os_error(), Some(c) if c == ERROR_ACCESS_DENIED as i32
                            || c == ERROR_DELETE_PENDING as i32)
                            && !entry.dir.path.exists() =>
                    {
                        pending.remove(index - 1);
                        shared.changed.notify_one();
                    }
                    Err(e) => return fail(shared, e),
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
        pub fn retarget(&mut self, _: &Targets) -> io::Result<bool> {
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
        std::fs::canonicalize(dir).unwrap()
    }

    async fn fires(events: &mut Events) -> bool {
        tokio::time::timeout(Duration::from_millis(500), events.changed())
            .await
            .is_ok()
    }

    /// What the watcher does: wait, retarget, then look. Returns whether it woke.
    async fn wake(events: &mut Events, t: &Targets) -> bool {
        let woke = fires(events).await;
        if woke {
            events.retarget(t);
        }
        woke
    }

    /// Wakes until quiet, so the next assertion sees only new events.
    async fn settle(events: &mut Events, t: &Targets) {
        while wake(events, t).await {}
    }

    fn native(t: &Targets) -> Events {
        // Directly, so a setup failure shows its error instead of falling back.
        Events {
            watch: Some(sys::Watch::new(t).expect("file change notifications")),
            rearm: false,
        }
    }

    #[test]
    fn shared_folder_merges_roles() {
        let root = temp();
        let t = Targets {
            config: root.join("auth/config.yaml"),
            auth_dir: root.join("auth"),
            files: Vec::new(),
        };
        // Other entries can appear only for symlinks on the way (Wine links C:\users).
        let d = dirs(&t);
        let auth = d.iter().find(|d| d.path == root.join("auth")).expect("auth folder");
        assert!(!d.iter().any(|d| d.path == root), "{d:?}");
        assert!(auth.matters("config.yaml".as_ref()));
        assert!(auth.matters("a.JSON".as_ref()));
        assert!(!auth.matters("notes.txt".as_ref()));
        // A missing auth folder is watched through its nearest existing ancestor, for
        // the first missing component only.
        let t = Targets {
            config: root.join("config.yaml"),
            auth_dir: root.join("later/auth"),
            files: Vec::new(),
        };
        let d = dirs(&t);
        let top = d.iter().find(|d| d.path == root).expect("ancestor watched");
        assert!(!d.iter().any(|d| d.path.starts_with(root.join("later"))), "{d:?}");
        assert!(top.matters("later".as_ref()));
        assert!(top.matters("config.yaml".as_ref()));
        assert!(!top.matters("notes.txt".as_ref()));
        // A missing config folder too.
        let t = Targets {
            config: root.join("gone/config.yaml"),
            auth_dir: root.join("auth"),
            files: Vec::new(),
        };
        let d = dirs(&t);
        let top = d.iter().find(|d| d.path == root).expect("ancestor watched");
        assert!(top.matters("gone".as_ref()) && !top.matters("config.yaml".as_ref()));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn relative_missing_auth_dir_watches_the_working_directory() {
        let t = Targets {
            config: PathBuf::from("config.yaml"),
            auth_dir: PathBuf::from(format!("missing-auths-{}", uuid::Uuid::new_v4())),
            files: Vec::new(),
        };
        let cwd = std::fs::canonicalize(".").unwrap();
        let d = dirs(&t);
        let here = d.iter().find(|d| d.path == cwd).expect("working directory watched");
        assert!(here.matters(t.auth_dir.as_os_str()));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_on_the_way_are_watched_by_name() {
        let root = temp();
        // Kubernetes Secret layout: x.json -> ..data/x.json, ..data -> ..v1.
        let secrets = root.join("secrets");
        std::fs::create_dir_all(secrets.join("..v1")).unwrap();
        std::fs::write(secrets.join("..v1/x.json"), "{}").unwrap();
        std::os::unix::fs::symlink("..v1", secrets.join("..data")).unwrap();
        std::os::unix::fs::symlink("..data/x.json", secrets.join("x.json")).unwrap();
        let t = Targets {
            config: root.join("config.yaml"),
            auth_dir: secrets.clone(),
            files: vec![secrets.join("x.json")],
        };
        let d = dirs(&t);
        let auth = d.iter().find(|d| d.path == secrets).unwrap();
        assert!(auth.json && auth.matters("..data".as_ref()), "{d:?}");
        assert!(d.iter().any(|d| d.path == secrets.join("..v1")), "{d:?}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn short_aliases_count_as_possible_matches() {
        assert!(sys::maybe_short_alias("CONFIG~1.YAM".as_ref()));
        assert!(!sys::maybe_short_alias("notes.txt".as_ref()));
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
        let mut events = native(&t);
        settle(&mut events, &t).await;
        assert!(!fires(&mut events).await, "no event while nothing changes");
        // An unrelated file next to the config is ignored (kqueue sees only folders).
        std::fs::write(root.join("notes.txt"), "x").unwrap();
        if cfg!(not(target_os = "macos")) {
            assert!(!fires(&mut events).await, "unrelated file woke the watcher");
        }
        settle(&mut events, &t).await;
        std::fs::write(&t.config, "port: 2\n").unwrap();
        assert!(wake(&mut events, &t).await, "config edit");
        settle(&mut events, &t).await;
        std::fs::write(root.join("auth/a.json"), r#"{"a":1}"#).unwrap();
        assert!(wake(&mut events, &t).await, "auth file edit in place");
        settle(&mut events, &t).await;
        std::fs::write(root.join("auth/b.json"), "{}").unwrap();
        assert!(wake(&mut events, &t).await, "new auth file");
        // An atomically replaced config is still watched.
        settle(&mut events, &t).await;
        let tmp = root.join("config.yaml.tmp");
        std::fs::write(&tmp, "port: 3\n").unwrap();
        std::fs::rename(&tmp, &t.config).unwrap();
        assert!(wake(&mut events, &t).await, "config replaced");
        settle(&mut events, &t).await;
        std::fs::write(&t.config, "port: 4\n").unwrap();
        assert!(wake(&mut events, &t).await, "edit after replacement");
        // A moved auth folder is followed.
        settle(&mut events, &t).await;
        std::fs::create_dir_all(root.join("auth2")).unwrap();
        t.auth_dir = root.join("auth2");
        t.files.clear();
        events.retarget(&t);
        settle(&mut events, &t).await;
        std::fs::write(root.join("auth2/c.json"), "{}").unwrap();
        assert!(wake(&mut events, &t).await, "new auth folder");
        drop(events);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A missing auth folder is created after start, then a login lands in it.
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[tokio::test]
    async fn follows_an_auth_folder_created_later() {
        let root = temp();
        let t = Targets {
            config: root.join("config.yaml"),
            auth_dir: root.join("later/auths"),
            files: Vec::new(),
        };
        std::fs::write(&t.config, "port: 1\n").unwrap();
        let mut events = native(&t);
        settle(&mut events, &t).await;
        std::fs::create_dir_all(&t.auth_dir).unwrap();
        assert!(wake(&mut events, &t).await, "auth folder created");
        settle(&mut events, &t).await;
        std::fs::write(t.auth_dir.join("claude-a.json"), "{}").unwrap();
        assert!(wake(&mut events, &t).await, "login in the new folder");
        drop(events);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// `rm -rf auths && cp -r backup auths`, then `mv auths.new auths`: each time the
    /// new folder is watched.
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[tokio::test]
    async fn follows_a_replaced_auth_folder() {
        let root = temp();
        let t = Targets {
            config: root.join("config.yaml"),
            auth_dir: root.join("auth"),
            files: Vec::new(),
        };
        std::fs::write(&t.config, "port: 1\n").unwrap();
        let mut events = native(&t);
        settle(&mut events, &t).await;
        std::fs::remove_dir_all(&t.auth_dir).unwrap();
        // Without POSIX delete semantics (FAT, network shares, Wine) a deleted folder
        // stays delete-pending until the watch closes its handle; the watcher must let
        // go of it for the name to be free again.
        let mut retries = 0;
        while let Err(e) = std::fs::create_dir(&t.auth_dir) {
            assert!(retries < 40 && e.kind() == std::io::ErrorKind::AlreadyExists, "{e}");
            retries += 1;
            let _ = wake(&mut events, &t).await;
        }
        assert!(wake(&mut events, &t).await, "folder replaced");
        settle(&mut events, &t).await;
        std::fs::write(t.auth_dir.join("a.json"), "{}").unwrap();
        assert!(wake(&mut events, &t).await, "file in the replaced folder");
        settle(&mut events, &t).await;
        std::fs::create_dir(root.join("auth.new")).unwrap();
        settle(&mut events, &t).await;
        std::fs::rename(&t.auth_dir, root.join("auth.old")).unwrap();
        std::fs::rename(root.join("auth.new"), &t.auth_dir).unwrap();
        assert!(wake(&mut events, &t).await, "folder renamed into place");
        settle(&mut events, &t).await;
        std::fs::write(t.auth_dir.join("b.json"), "{}").unwrap();
        assert!(wake(&mut events, &t).await, "file in the renamed folder");
        drop(events);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A config bind-mounted as a single file: the write goes through another path to
    /// the same inode and notifies only the file (a hard link stands in for the mount).
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn sees_writes_to_the_config_through_another_path() {
        let root = temp();
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::create_dir_all(root.join("b")).unwrap();
        let t = Targets {
            config: root.join("a/config.yaml"),
            auth_dir: root.join("auth"),
            files: Vec::new(),
        };
        std::fs::write(&t.config, "port: 1\n").unwrap();
        std::fs::hard_link(&t.config, root.join("b/config.yaml")).unwrap();
        let mut events = native(&t);
        settle(&mut events, &t).await;
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(root.join("b/config.yaml"))
            .unwrap();
        f.write_all(b"port: 2\n").unwrap();
        drop(f);
        assert!(wake(&mut events, &t).await, "in-place write through another path");
        drop(events);
        std::fs::remove_dir_all(root).unwrap();
    }
}
