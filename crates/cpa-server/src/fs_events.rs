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
//! system (NFS, 9p such as WSL2's /mnt/c, sshfs) raises no event here and is seen with
//! the next local change; on an SMB/CIFS share it may not be reported, depending on the
//! server.
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
    /// The auth files seen last; kqueue watches each one.
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
    /// Writes to entries matter (the auth folder, the config's folder, a symlinked
    /// file's real folder). Otherwise only entries appearing, vanishing or being renamed
    /// do (a missing folder's ancestor, a folder holding a symlink), so a busy home
    /// folder does not wake the watcher on every write.
    content: bool,
    /// Only on the way to a target (a folder holding a symlink, a symlinked file's real
    /// folder). A permission error skips it instead of turning notifications off (on
    /// Linux a real folder falls back to watching the file). On macOS, where a folder
    /// watch cannot filter by name, only the folder itself being renamed or removed is
    /// watched, next to each link and file inside it.
    optional: bool,
}

/// Why a folder is watched; see [`Dir`].
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    /// The auth folder.
    Auth,
    /// The config file's folder.
    Content,
    /// A symlinked auth file's real folder.
    RealParent,
    /// The nearest existing ancestor of a missing folder.
    Ancestor,
    /// The folder holding a symlink on the way.
    Link,
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

type Add<'a> = dyn FnMut(PathBuf, Option<OsString>, Role) + 'a;

/// Watches the nearest existing ancestor of the missing folder `missing`, for the first
/// missing component only, so a busy ancestor such as the home folder wakes nothing
/// else. Each retarget moves the watch one level down as `mkdir -p` proceeds.
fn add_missing(missing: &Path, add: &mut Add<'_>) {
    let missing = dot(missing);
    if let Some(ancestor) = missing.ancestors().skip(1).find(|a| dot(a).is_dir()) {
        let first = missing
            .strip_prefix(ancestor)
            .ok()
            .and_then(|rest| rest.components().next())
            .map(|c| c.as_os_str().to_owned());
        add(dot(ancestor).to_owned(), first, Role::Ancestor);
    }
}

/// For each symlink on the way to `path`, watches the folder holding the link for the
/// link's name: a Kubernetes `..data` swap or an `ln -sfn` then wakes the watcher.
/// Cost: one lstat per path component, only when retargeting. Returns the path with
/// every link on the way resolved (it may not exist).
fn follow_links(path: &Path, add: &mut Add<'_>) -> PathBuf {
    let mut cur = PathBuf::new();
    let mut rest = path.to_owned();
    let mut hops = 0;
    loop {
        let mut components = rest.components();
        let Some(component) = components.next() else { return cur };
        let remaining = components.as_path().to_owned();
        match component {
            Component::Prefix(_) | Component::RootDir => cur.push(component.as_os_str()),
            Component::CurDir => {}
            // Only a resolved name can be popped: `../..` stays two levels up.
            Component::ParentDir => match cur.components().next_back() {
                Some(Component::Normal(_)) => {
                    cur.pop();
                }
                // A root stays; a bare drive prefix (`C:..\x`) is relative and keeps it.
                Some(Component::RootDir) => {}
                _ => cur.push(".."),
            },
            Component::Normal(name) => {
                let next = cur.join(name);
                let link = next.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink());
                if link && hops < 40 {
                    hops += 1;
                    add(dot(&cur).to_owned(), Some(name.to_owned()), Role::Link);
                    let Ok(target) = std::fs::read_link(&next) else {
                        return next;
                    };
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
    let mut add = |path: PathBuf, name: Option<OsString>, role: Role| {
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        let json = role == Role::Auth;
        let content = matches!(role, Role::Auth | Role::Content | Role::RealParent);
        let optional = matches!(role, Role::Link | Role::RealParent);
        match out.iter_mut().find(|d| d.path == path) {
            Some(d) => {
                d.json |= json;
                d.content |= content;
                d.optional &= optional;
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
                content,
                optional,
            }),
        }
    };
    let name = |p: &Path| p.file_name().map(ToOwned::to_owned);
    let folder = parent(&t.config);
    match name(&t.config) {
        Some(n) if folder.is_dir() => add(folder, Some(n), Role::Content),
        _ => add_missing(&folder, &mut add),
    }
    follow_links(&t.config, &mut add);
    if let Ok(real) = std::fs::canonicalize(&t.config)
        && let Some(n) = name(&real)
    {
        add(parent(&real), Some(n), Role::Content);
    }
    if t.auth_dir.is_dir() {
        add(t.auth_dir.clone(), None, Role::Auth);
    } else {
        add_missing(&t.auth_dir, &mut add);
    }
    follow_links(&t.auth_dir, &mut add);
    // Symlinked auth files, dangling ones included (the directory listing still has
    // them). Cost: one directory listing per retarget.
    for entry in std::fs::read_dir(&t.auth_dir).into_iter().flatten().flatten() {
        let file = entry.path();
        let json = file
            .file_name()
            .is_some_and(|n| n.to_string_lossy().to_lowercase().ends_with(".json"));
        if !json || !entry.file_type().is_ok_and(|f| f.is_symlink()) {
            continue;
        }
        let resolved = follow_links(&file, &mut add);
        match std::fs::canonicalize(&file) {
            // Linux sees the file's writes here; macOS watches the file through the link
            // (Targets::files) and here only the folder being renamed or removed.
            Ok(real) => {
                if let Some(n) = name(&real) {
                    add(parent(&real), Some(n), Role::RealParent);
                }
            }
            // Dangling: watch for its target to appear.
            Err(_) => add_missing(&resolved, &mut add),
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
    /// Folders where only entries appearing, vanishing or being renamed matter.
    const ENTRY_MASK: u32 = libc::IN_CREATE
        | libc::IN_DELETE
        | libc::IN_MOVED_FROM
        | libc::IN_MOVED_TO
        | libc::IN_DELETE_SELF
        | libc::IN_MOVE_SELF;

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

        /// Watches one file's own inode. IN_MASK_ADD: should the path be a folder that is
        /// watched too, its folder mask is kept.
        fn add_file(&self, path: &Path, next: &mut HashMap<i32, Vec<Dir>>) -> io::Result<()> {
            let wd = self.add(path, FILE_MASK | libc::IN_MASK_ADD)?;
            next.entry(wd).or_default().push(Dir {
                path: path.to_owned(),
                names: None,
                json: false,
                content: true,
                optional: false,
            });
            Ok(())
        }

        /// Adds every watch again: the kernel returns the existing descriptor for an inode
        /// it already watches (no event, so idle stays quiet) and a new one for a folder
        /// or file that was replaced. Descriptors no longer returned are removed. Returns
        /// whether the set changed.
        pub fn retarget(&mut self, targets: &Targets) -> io::Result<bool> {
            let mut next: HashMap<i32, Vec<Dir>> = HashMap::new();
            let mut vanished = false;
            let mut dirs = dirs(targets);
            // Entry-only folders first: adding a watch replaces the mask of the same inode,
            // so a full add after them wins where two paths share one.
            dirs.sort_by_key(|d| d.content);
            for dir in dirs {
                let mask = if dir.content { MASK } else { ENTRY_MASK };
                match self.add(&dir.path, mask | libc::IN_ONLYDIR) {
                    Ok(wd) => next.entry(wd).or_default().push(dir),
                    // Removed since `dirs` looked: the next wake retargets again.
                    Err(e) if e.raw_os_error() == Some(libc::ENOENT) => vanished = true,
                    // A symlinked auth file's real folder that cannot be listed: nothing
                    // else sees that file's writes here, so watch the file itself
                    // (inotify needs read permission on what it watches).
                    Err(e)
                        if dir.optional
                            && dir.content
                            && matches!(e.raw_os_error(), Some(libc::EACCES | libc::EPERM)) =>
                    {
                        for name in dir.names.iter().flatten() {
                            match self.add_file(&dir.path.join(name), &mut next) {
                                Err(e) if e.raw_os_error() == Some(libc::ENOENT) => vanished = true,
                                r => r?,
                            }
                        }
                    }
                    // A folder holding a symlink on the way that cannot be watched: its
                    // target's own folder still is.
                    Err(e)
                        if dir.optional
                            && matches!(e.raw_os_error(), Some(libc::EACCES | libc::EPERM | libc::ENOTDIR)) => {}
                    Err(e) => return Err(e),
                }
            }
            match self.add_file(&targets.config, &mut next) {
                // No file to watch; its folder sees it appear.
                Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR | libc::EACCES)) => {}
                r => r?,
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
        /// Each watched path, and whether it is a symlink watched as itself.
        open: HashMap<(PathBuf, bool), Registered>,
    }

    struct Registered {
        /// Closing it drops the kqueue registration.
        _fd: OwnedFd,
        /// Device and inode it was opened at.
        id: (u64, u64),
        /// What it was registered for, which decides the filter.
        kind: Kind,
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Kind {
        /// A folder whose entries matter.
        Folder,
        /// A folder only on the way: watched for being renamed or removed only, so a busy
        /// folder wakes nothing.
        Shell,
        /// A symlink on the way, watched as itself.
        Link,
        File,
    }

    /// Device and inode, of the link itself for `link`.
    fn identity(path: &std::path::Path, link: bool) -> Option<(u64, u64)> {
        let meta = if link {
            std::fs::symlink_metadata(path)
        } else {
            std::fs::metadata(path)
        };
        meta.ok().map(|m| (m.dev(), m.ino()))
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
        /// (another device and inode). Returns whether the watched set changed, so the
        /// caller looks once more: a change made before a new watch existed raised no
        /// event, and a dropped watch may have missed one.
        pub fn retarget(&mut self, targets: &Targets) -> io::Result<bool> {
            let mut want: Vec<(PathBuf, Kind)> = Vec::new();
            for dir in dirs(targets) {
                if dir.optional {
                    // A folder only on the way: kqueue reports a rename to the renamed
                    // folder and its parent, not to the links and files inside, so the
                    // folder is watched for that alone, and each link in it as itself.
                    // A symlinked auth file's own writes come from its File watch.
                    for name in dir.names.iter().flatten() {
                        let path = dir.path.join(name);
                        if path.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink()) {
                            want.push((path, Kind::Link));
                        }
                    }
                    want.push((dir.path, Kind::Shell));
                } else {
                    want.push((dir.path, Kind::Folder));
                }
            }
            want.push((targets.config.clone(), Kind::File));
            if let Ok(real) = std::fs::canonicalize(&targets.config) {
                want.push((real, Kind::File));
            }
            want.extend(targets.files.iter().map(|f| (f.clone(), Kind::File)));
            let order = |(p, k): &(PathBuf, Kind)| (p.clone(), *k == Kind::Link);
            want.sort_by(|a, b| (&a.0, a.1 == Kind::Link).cmp(&(&b.0, b.1 == Kind::Link)));
            want.dedup_by(|a, b| a.0 == b.0 && (a.1 == Kind::Link) == (b.1 == Kind::Link));
            // Leave at least half the descriptors to sockets and logs; past that, the
            // 2-second poll is the better trade.
            let limit = soft_limit().min(max_files_per_proc());
            if want.len() as libc::rlim_t > limit / 2 {
                return Err(io::Error::from_raw_os_error(libc::EMFILE));
            }
            let kind_of = |k: &(PathBuf, bool)| want.binary_search_by(|w| order(w).cmp(k)).ok().map(|i| want[i].1);
            let before = self.open.len();
            // Closing a descriptor drops its kqueue registration. A path whose role
            // changed (a real folder that now waits for a dangling link's target, or the
            // reverse) is registered again with the other filter.
            self.open
                .retain(|k, r| kind_of(k) == Some(r.kind) && identity(&k.0, k.1) == Some(r.id));
            let mut changed = self.open.len() != before;
            for (path, kind) in &want {
                let link = *kind == Kind::Link;
                let k = (path.clone(), link);
                if self.open.contains_key(&k) {
                    continue;
                }
                let Some(id) = identity(path, link) else { continue };
                let c = CString::new(path.as_os_str().as_bytes())?;
                let flags = libc::O_EVTONLY | libc::O_CLOEXEC | if link { libc::O_SYMLINK } else { 0 };
                // SAFETY: `c` is a valid C string for the duration of the call.
                let fd = unsafe { libc::open(c.as_ptr(), flags) };
                if fd < 0 {
                    let e = io::Error::last_os_error();
                    let code = e.raw_os_error();
                    let skip = e.kind() == io::ErrorKind::NotFound
                        // An unreadable auth file is skipped, not a reason to poll.
                        || (*kind == Kind::File && matches!(code, Some(libc::EACCES | libc::EPERM)))
                        // So is a folder or link only on the way to a target.
                        || (matches!(kind, Kind::Shell | Kind::Link)
                            && matches!(code, Some(libc::EACCES | libc::EPERM | libc::ENOTDIR)));
                    if skip {
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
                change.fflags = match kind {
                    // The link itself: replaced (`ln -sfn`, a `..data` swap) or removed.
                    Kind::Link => libc::NOTE_DELETE | libc::NOTE_RENAME | libc::NOTE_ATTRIB | libc::NOTE_REVOKE,
                    Kind::Shell => libc::NOTE_DELETE | libc::NOTE_RENAME | libc::NOTE_REVOKE,
                    Kind::Folder | Kind::File => {
                        libc::NOTE_WRITE
                            | libc::NOTE_EXTEND
                            | libc::NOTE_ATTRIB
                            | libc::NOTE_DELETE
                            | libc::NOTE_RENAME
                            | libc::NOTE_REVOKE
                    }
                };
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
                self.open.insert(
                    k,
                    Registered {
                        _fd: fd,
                        id,
                        kind: *kind,
                    },
                );
                changed = true;
            }
            // A required folder that vanished between listing and opening: look again,
            // which lists its ancestor instead. Files and optional folders are left out,
            // so one that stays missing or unreadable cannot make the watcher spin.
            changed |= want
                .iter()
                .any(|(p, k)| *k == Kind::Folder && !self.open.contains_key(&(p.clone(), false)));
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
        NTSTATUS, STATUS_DELETE_PENDING, WAIT_FAILED, WAIT_OBJECT_0,
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
                return Err(open_error());
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

    /// The parents of the watched folders (folder renames only), then the folders. The
    /// parents open first, so a folder renamed into place after its open fails is still
    /// seen. A folder that is missing (between the two renames of a swap) is skipped
    /// until its parent's next event; a parent that cannot be opened only loses that
    /// safety net.
    fn open_all(dirs: &[Dir]) -> io::Result<Vec<Pending>> {
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
                    content: false,
                    optional: true,
                }),
            }
        }
        let mut pending: Vec<Pending> = parents
            .iter()
            .filter_map(|p| Pending::open(p, PARENT_FILTER, true).ok())
            .collect();
        for dir in dirs {
            let parent_armed = pending
                .iter()
                .any(|p| p.parent && Some(p.dir.path.as_path()) == dir.path.parent());
            match Pending::open(dir, FILTER, false) {
                Ok(p) => pending.push(p),
                // Gone (renamed away or deleted): the caller's next retarget, which the
                // reopen notification triggers, watches its nearest ancestor instead.
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                // Delete-pending (a legacy delete still held open elsewhere): the name
                // still lists, so only the parent's event when it finally goes tells. A
                // folder that denies access stays an error (the 2-second poll).
                Err(e) if parent_armed && e.raw_os_error() == Some(ERROR_DELETE_PENDING as i32) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(pending)
    }

    /// Access denied or delete-pending on a folder that was opened, or whose parent
    /// could be: the folder is being deleted. An open handle's access never changes, and
    /// the name still lists until the last handle closes, so the path cannot tell.
    fn gone(e: &io::Error) -> bool {
        matches!(e.raw_os_error(), Some(c) if c == ERROR_ACCESS_DENIED as i32 || c == ERROR_DELETE_PENDING as i32)
    }

    // Not in windows-sys. LLVM's Windows Path.inc reads it the same way.
    #[link(name = "ntdll", kind = "raw-dylib")]
    unsafe extern "system" {
        fn RtlGetLastNtStatus() -> NTSTATUS;
    }

    /// The error of a CreateFileW that just failed. Win32 maps both a delete-pending
    /// folder and one whose ACL denies the access asked for (CreateFileW always adds
    /// SYNCHRONIZE and FILE_READ_ATTRIBUTES) to ERROR_ACCESS_DENIED; the NTSTATUS still
    /// tells them apart. Call it before anything else can set the thread's last error.
    fn open_error() -> io::Error {
        // SAFETY: reads the calling thread's last NTSTATUS; no arguments.
        if unsafe { RtlGetLastNtStatus() } == STATUS_DELETE_PENDING {
            return io::Error::from_raw_os_error(ERROR_DELETE_PENDING as i32);
        }
        io::Error::last_os_error()
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
                    // The folder itself is being deleted: let go of it (with legacy delete
                    // semantics this handle is what keeps it) and look; its parent's
                    // event then reopens, and the caller's retarget watches the nearest
                    // existing ancestor.
                    Err(e) if gone(&e) => {
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

    /// A symlinked auth file whose real folder lies outside every other watched folder.
    #[cfg(unix)]
    fn linked_file_layout() -> (PathBuf, Targets) {
        let root = temp();
        std::fs::create_dir_all(root.join("conf")).unwrap();
        std::fs::create_dir_all(root.join("real")).unwrap();
        std::fs::write(root.join("real/x.json"), "{}").unwrap();
        std::os::unix::fs::symlink(root.join("real/x.json"), root.join("auth/x.json")).unwrap();
        let t = Targets {
            config: root.join("conf/config.yaml"),
            auth_dir: root.join("auth"),
            files: vec![root.join("auth/x.json")],
        };
        (root, t)
    }

    /// The real folder of a symlinked auth file: renaming it wakes the watcher (on macOS
    /// through a rename-only folder watch), unrelated writes in it do not, and the file's
    /// own writes do.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn watches_a_linked_files_real_folder() {
        let (root, t) = linked_file_layout();
        let mut events = native(&t);
        settle(&mut events, &t).await;
        std::fs::write(root.join("real/notes.txt"), "x").unwrap();
        assert!(!fires(&mut events).await, "unrelated file in the real folder");
        std::fs::write(root.join("real/x.json"), r#"{"a":1}"#).unwrap();
        assert!(wake(&mut events, &t).await, "linked file edited in place");
        settle(&mut events, &t).await;
        std::fs::rename(root.join("real"), root.join("real.old")).unwrap();
        assert!(wake(&mut events, &t).await, "real folder renamed away");
        drop(events);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A symlinked auth file's target is deleted and recreated: its folder changes role
    /// (it waits for the target, then only lies on the way again), and each change of
    /// role takes effect, so the target's return wakes the watcher and an unrelated
    /// file in the folder afterwards does not.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn follows_a_linked_files_folder_changing_role() {
        let (root, t) = linked_file_layout();
        let mut events = native(&t);
        settle(&mut events, &t).await;
        std::fs::remove_file(root.join("real/x.json")).unwrap();
        settle(&mut events, &t).await;
        std::fs::write(root.join("real/x.json"), "{}").unwrap();
        assert!(wake(&mut events, &t).await, "linked file's target recreated");
        settle(&mut events, &t).await;
        std::fs::write(root.join("real/notes.txt"), "x").unwrap();
        assert!(!fires(&mut events).await, "unrelated file in the real folder");
        drop(events);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A real folder that cannot be listed (no read permission): the file itself is
    /// watched instead, so its in-place edits are still seen.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn watches_a_linked_file_whose_folder_cannot_be_listed() {
        use std::os::unix::fs::PermissionsExt as _;
        let (root, t) = linked_file_layout();
        let real = root.join("real");
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o311)).unwrap();
        if std::fs::read_dir(&real).is_ok() {
            std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::remove_dir_all(root).unwrap();
            // CI sets CPA_TEST_NO_SKIP: a run as root must not pass without the check.
            assert!(
                std::env::var_os("CPA_TEST_NO_SKIP").is_none(),
                "folder permissions are not enforced (running as root?), and CPA_TEST_NO_SKIP is set"
            );
            return;
        }
        let mut events = native(&t);
        settle(&mut events, &t).await;
        std::fs::write(real.join("x.json"), r#"{"a":1}"#).unwrap();
        let woke = wake(&mut events, &t).await;
        drop(events);
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::remove_dir_all(root).unwrap();
        assert!(woke, "in-place edit of a file in an unlistable folder");
    }

    /// A relative path on a drive (`C:..\x`) keeps its `..`: a bare drive prefix is not
    /// a root.
    #[cfg(windows)]
    #[test]
    fn drive_relative_parent_dirs_are_kept() {
        let mut add = |_: PathBuf, _: Option<OsString>, _: Role| {};
        assert_eq!(follow_links(Path::new(r"C:..\x"), &mut add), PathBuf::from(r"C:..\x"));
        assert_eq!(follow_links(Path::new(r"C:\..\x"), &mut add), PathBuf::from(r"C:\x"));
    }

    /// Opens `path` (file or folder) with `access`, sharing everything.
    #[cfg(windows)]
    fn open_raw(path: &Path, access: u32) -> std::io::Result<windows_sys::Win32::Foundation::HANDLE> {
        use std::os::windows::ffi::OsStrExt as _;
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
            OPEN_EXISTING,
        };
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        // SAFETY: `wide` is NUL-terminated and outlives the call.
        let h = unsafe {
            CreateFileW(
                wide.as_ptr(),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                std::ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(h)
        }
    }

    #[cfg(windows)]
    fn close_raw(h: windows_sys::Win32::Foundation::HANDLE) {
        // SAFETY: a handle from open_raw, closed once.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(h) };
    }

    /// Deletes an empty folder with legacy semantics (FileDispositionInfo, as on FAT,
    /// network shares and older tools): it stays delete-pending while any handle to it
    /// is open.
    #[cfg(windows)]
    fn legacy_delete(path: &Path) {
        use windows_sys::Win32::Storage::FileSystem::{
            DELETE, FILE_DISPOSITION_INFO, FileDispositionInfo, SetFileInformationByHandle,
        };
        let h = open_raw(path, DELETE).unwrap();
        let info = FILE_DISPOSITION_INFO { DeleteFile: true };
        // SAFETY: `info` outlives the call; `h` is open.
        let ok = unsafe {
            SetFileInformationByHandle(
                h,
                FileDispositionInfo,
                (&raw const info).cast(),
                std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
            )
        };
        let e = std::io::Error::last_os_error();
        close_raw(h);
        assert_ne!(ok, 0, "{e}");
    }

    /// A legacy delete of the auth folder: the folder stays delete-pending while the
    /// watch's own handle is open. The watch lets go (the failed read, in `run`), keeps
    /// running and follows the recreated folder.
    #[cfg(windows)]
    #[tokio::test]
    async fn survives_a_legacy_delete_of_the_auth_folder() {
        let root = temp();
        let t = Targets {
            config: root.join("config.yaml"),
            auth_dir: root.join("auth"),
            files: Vec::new(),
        };
        std::fs::write(&t.config, "port: 1\n").unwrap();
        let mut events = native(&t);
        settle(&mut events, &t).await;
        legacy_delete(&t.auth_dir);
        assert!(wake(&mut events, &t).await, "auth folder deleted");
        settle(&mut events, &t).await;
        assert!(events.watch.is_some(), "notifications turned off");
        assert!(!t.auth_dir.exists(), "the watch still holds the deleted folder");
        std::fs::create_dir(&t.auth_dir).unwrap();
        settle(&mut events, &t).await;
        std::fs::write(t.auth_dir.join("a.json"), "{}").unwrap();
        assert!(wake(&mut events, &t).await, "login after the folder came back");
        assert!(events.watch.is_some(), "notifications turned off");
        drop(events);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// The same, with another program still holding the deleted folder open, and a
    /// reopen of every folder while it is delete-pending: `open_all` skips it (its
    /// parent is watched) instead of turning notifications off, and its parent's event
    /// when the last handle closes is followed.
    #[cfg(windows)]
    #[tokio::test]
    async fn skips_an_auth_folder_held_delete_pending() {
        let root = temp();
        let t = Targets {
            config: root.join("config.yaml"),
            auth_dir: root.join("auth"),
            files: Vec::new(),
        };
        std::fs::write(&t.config, "port: 1\n").unwrap();
        let mut events = native(&t);
        settle(&mut events, &t).await;
        let holder = open_raw(&t.auth_dir, 0).unwrap();
        legacy_delete(&t.auth_dir);
        let _ = wake(&mut events, &t).await;
        settle(&mut events, &t).await;
        // A changed folder list makes the thread reopen every folder now.
        let other = Targets {
            config: root.join("other.yaml"),
            ..t.clone()
        };
        // The delete-pending folder still lists, so it is among the folders reopened.
        let listed = dirs(&other).iter().any(|d| d.path == t.auth_dir);
        events.retarget(&other);
        let skipped = events.watch.is_some();
        close_raw(holder);
        assert!(listed, "the delete-pending folder was not in the reopened list");
        assert!(skipped, "a delete-pending folder turned notifications off");
        assert!(wake(&mut events, &other).await, "the held folder finally went");
        settle(&mut events, &other).await;
        std::fs::create_dir(&t.auth_dir).unwrap();
        settle(&mut events, &other).await;
        std::fs::write(t.auth_dir.join("a.json"), "{}").unwrap();
        assert!(wake(&mut events, &other).await, "login after the folder came back");
        assert!(events.watch.is_some(), "notifications turned off");
        drop(events);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A config folder whose ACL denies Everyone `deny` (on the folder only, so
    /// config.yaml keeps its own ACL) is never left watched by nobody. Whether the deny
    /// takes effect depends on the account (an administrator's privileges can open the
    /// folder anyway), so the test checks the outcome that matches what a zero-access
    /// open finds, without requiring either:
    ///
    /// - denied (as on a delete-pending folder): setting up notifications must fail,
    ///   so the watcher polls instead of skipping the folder;
    /// - allowed: notifications either fail (the folder cannot be listed) or watch it,
    ///   and then an in-place edit of config.yaml wakes the watcher.
    #[cfg(windows)]
    async fn unlistable_config_folder(deny: &str) {
        let root = temp();
        let conf = root.join("conf");
        std::fs::create_dir(&conf).unwrap();
        std::fs::write(conf.join("config.yaml"), "port: 1\n").unwrap();
        // icacls does not take the `\\?\` form canonicalize returns.
        let plain = conf.to_string_lossy().trim_start_matches(r"\\?\").to_owned();
        let icacls = |args: &[&str]| {
            std::process::Command::new("icacls")
                .arg(&plain)
                .args(args)
                .status()
                .is_ok_and(|s| s.success())
        };
        assert!(icacls(&["/deny", &format!("*S-1-1-0:({deny})")]), "icacls /deny");
        let t = Targets {
            config: conf.join("config.yaml"),
            auth_dir: root.join("auth"),
            files: Vec::new(),
        };
        // Nothing below panics until the ACL is restored.
        let acl = std::process::Command::new("icacls")
            .arg(&plain)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        let denied = open_raw(&conf, 0)
            .map(close_raw)
            .is_err_and(|e| e.kind() == std::io::ErrorKind::PermissionDenied);
        let outcome = match sys::Watch::new(&t) {
            Err(e) => Err(e),
            Ok(watch) => {
                let mut events = Events {
                    watch: Some(watch),
                    rearm: false,
                };
                settle(&mut events, &t).await;
                let written = std::fs::write(&t.config, "port: 2\n").is_ok();
                Ok(written && wake(&mut events, &t).await)
            }
        };
        let restored = icacls(&["/remove:d", "*S-1-1-0"]);
        assert!(restored, "icacls /remove:d");
        std::fs::remove_dir_all(root).unwrap();
        let case = format!("({deny}), zero-access open denied: {denied}; ACL:\n{acl}");
        match outcome {
            Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied, "{e}; {case}"),
            Ok(_) if denied => panic!("a folder denying even a zero-access open was skipped; {case}"),
            Ok(woke) => assert!(woke, "the folder was left unwatched; {case}"),
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn an_unlistable_config_folder_is_never_left_unwatched() {
        // List folder / read data only.
        unlistable_config_folder("RD").await;
        // Generic read, and SYNCHRONIZE by name: CreateFileW adds SYNCHRONIZE to every
        // open, so where the deny takes effect even a zero-access open is refused, as on
        // a delete-pending folder.
        unlistable_config_folder("R").await;
        unlistable_config_folder("RD,S").await;
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

    /// Renaming the watched auth folder away, or deleting it, keeps notifications on:
    /// the recreated folder is watched again and a login in it is seen.
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[tokio::test]
    async fn survives_the_auth_folder_going_away() {
        for rename in [true, false] {
            let root = temp();
            let t = Targets {
                config: root.join("config.yaml"),
                auth_dir: root.join("auth"),
                files: Vec::new(),
            };
            std::fs::write(&t.config, "port: 1\n").unwrap();
            let mut events = native(&t);
            settle(&mut events, &t).await;
            if rename {
                std::fs::rename(&t.auth_dir, root.join("auth.old")).unwrap();
            } else {
                std::fs::remove_dir_all(&t.auth_dir).unwrap();
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
            settle(&mut events, &t).await;
            // A delete-pending folder (no POSIX delete semantics) frees its name once the
            // watch lets go of it.
            let mut retries = 0;
            while let Err(e) = std::fs::create_dir(&t.auth_dir) {
                assert!(retries < 40 && e.kind() == std::io::ErrorKind::AlreadyExists, "{e}");
                retries += 1;
                let _ = wake(&mut events, &t).await;
            }
            settle(&mut events, &t).await;
            std::fs::write(t.auth_dir.join("a.json"), "{}").unwrap();
            assert!(
                wake(&mut events, &t).await,
                "login after the folder came back (rename: {rename})"
            );
            assert!(events.watch.is_some(), "notifications turned off (rename: {rename})");
            drop(events);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    /// `-config ../../current/config.yaml` with `current` a release symlink: the folder
    /// holding the link is watched, and `ln -sfn` to a new release wakes the watcher.
    #[cfg(unix)]
    #[tokio::test]
    async fn follows_a_release_link_behind_leading_parent_dirs() {
        let root = temp();
        std::fs::create_dir_all(root.join("rel1")).unwrap();
        std::fs::create_dir_all(root.join("rel2")).unwrap();
        std::fs::write(root.join("rel1/config.yaml"), "port: 1\n").unwrap();
        std::fs::write(root.join("rel2/config.yaml"), "port: 2\n").unwrap();
        std::os::unix::fs::symlink("rel1", root.join("current")).unwrap();
        let cwd = std::env::current_dir().unwrap();
        let depth = cwd.components().filter(|c| matches!(c, Component::Normal(_))).count();
        let relative = PathBuf::from("../".repeat(depth))
            .join(root.strip_prefix("/").unwrap())
            .join("current/config.yaml");
        assert!(depth >= 2 && relative.is_file(), "{relative:?}");
        let t = Targets {
            config: relative,
            auth_dir: root.join("auth"),
            files: Vec::new(),
        };
        let d = dirs(&t);
        let holder = d.iter().find(|d| d.path == root).expect("link folder watched");
        assert!(holder.matters("current".as_ref()), "{d:?}");
        let mut events = native(&t);
        settle(&mut events, &t).await;
        // `ln -sfn rel2 current`: a new link renamed over the old one.
        std::os::unix::fs::symlink("rel2", root.join("current.new")).unwrap();
        std::fs::rename(root.join("current.new"), root.join("current")).unwrap();
        assert!(wake(&mut events, &t).await, "release switch");
        drop(events);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A dangling symlinked auth file: the nearest existing folder on the way to its
    /// target is watched for the missing name.
    #[cfg(unix)]
    #[test]
    fn dangling_auth_links_watch_for_their_target() {
        let root = temp();
        std::os::unix::fs::symlink(root.join("later/x.json"), root.join("auth/x.json")).unwrap();
        let t = Targets {
            config: root.join("auth/config.yaml"),
            auth_dir: root.join("auth"),
            files: Vec::new(),
        };
        let d = dirs(&t);
        let top = d.iter().find(|d| d.path == root).expect("target's ancestor watched");
        assert!(top.matters("later".as_ref()) && !top.content, "{d:?}");
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
