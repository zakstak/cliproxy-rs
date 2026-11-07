//! Wakes the config watcher when `config.yaml` or the top level of `auth-dir` may have
//! changed, so an idle server does no file work at all. One inotify descriptor watches
//! the config file's folder, the config file itself (so an in-place edit through a bind
//! mount of the single file is seen) and the auth folder, registered with tokio's
//! reactor, without an extra thread. This fork builds for Linux only.
//!
//! An event only says "look again": the watcher still compares content hashes and
//! waits for writes to settle. After each wake the caller retargets ([`Events::retarget`])
//! before it looks, so replaced, moved or newly created folders are followed. When inotify
//! cannot be set up (inotify or descriptor limits), it falls back to looking every 2 seconds and logs
//! that once. As in Go, a change made by another machine on a network or FUSE file
//! system (NFS, 9p such as WSL2's /mnt/c, sshfs) raises no event here and is seen with
//! the next local change; on an SMB/CIFS share it may not be reported, depending on the
//! server.
use cpa_core::gostr::GoStr;
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
}

/// One folder (or the config file) to watch and which of its entries matter.
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
    /// folder). A permission error skips it instead of turning notifications off (a real
    /// folder falls back to watching the file).
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
    fn matters(&self, name: &std::ffi::OsStr) -> bool {
        let lossy = name.to_string_lossy();
        (self.json && lossy.go_lower().ends_with(".json"))
            || self.names.as_ref().is_none_or(|names| names.iter().any(|n| n == name))
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
                // A root stays.
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
            .is_some_and(|n| n.to_string_lossy().go_lower().ends_with(".json"));
        if !json || !entry.file_type().is_ok_and(|f| f.is_symlink()) {
            continue;
        }
        let resolved = follow_links(&file, &mut add);
        match std::fs::canonicalize(&file) {
            // The file's writes are seen through its real folder.
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

    /// Follows moved, replaced or newly created folders and a replaced config file. Call
    /// after every wake, before looking, and after each applied change. Blocking (file
    /// metadata).
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
        };
        let cwd = std::fs::canonicalize(".").unwrap();
        let d = dirs(&t);
        let here = d.iter().find(|d| d.path == cwd).expect("working directory watched");
        assert!(here.matters(t.auth_dir.as_os_str()));
    }

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
        };
        let d = dirs(&t);
        let auth = d.iter().find(|d| d.path == secrets).unwrap();
        assert!(auth.json && auth.matters("..data".as_ref()), "{d:?}");
        assert!(d.iter().any(|d| d.path == secrets.join("..v1")), "{d:?}");
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A symlinked auth file whose real folder lies outside every other watched folder.
    fn linked_file_layout() -> (PathBuf, Targets) {
        let root = temp();
        std::fs::create_dir_all(root.join("conf")).unwrap();
        std::fs::create_dir_all(root.join("real")).unwrap();
        std::fs::write(root.join("real/x.json"), "{}").unwrap();
        std::os::unix::fs::symlink(root.join("real/x.json"), root.join("auth/x.json")).unwrap();
        let t = Targets {
            config: root.join("conf/config.yaml"),
            auth_dir: root.join("auth"),
        };
        (root, t)
    }

    /// The real folder of a symlinked auth file: renaming it wakes the watcher, unrelated writes in it do not, and the file's
    /// own writes do.
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

    #[tokio::test]
    async fn wakes_for_watched_files_only() {
        let root = temp();
        let mut t = Targets {
            config: root.join("config.yaml"),
            auth_dir: root.join("auth"),
        };
        std::fs::write(&t.config, "port: 1\n").unwrap();
        std::fs::write(root.join("auth/a.json"), "{}").unwrap();
        let mut events = native(&t);
        settle(&mut events, &t).await;
        assert!(!fires(&mut events).await, "no event while nothing changes");
        // An unrelated file next to the config is ignored.
        std::fs::write(root.join("notes.txt"), "x").unwrap();
        assert!(!fires(&mut events).await, "unrelated file woke the watcher");
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
        events.retarget(&t);
        settle(&mut events, &t).await;
        std::fs::write(root.join("auth2/c.json"), "{}").unwrap();
        assert!(wake(&mut events, &t).await, "new auth folder");
        drop(events);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A missing auth folder is created after start, then a login lands in it.
    #[tokio::test]
    async fn follows_an_auth_folder_created_later() {
        let root = temp();
        let t = Targets {
            config: root.join("config.yaml"),
            auth_dir: root.join("later/auths"),
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
    #[tokio::test]
    async fn follows_a_replaced_auth_folder() {
        let root = temp();
        let t = Targets {
            config: root.join("config.yaml"),
            auth_dir: root.join("auth"),
        };
        std::fs::write(&t.config, "port: 1\n").unwrap();
        let mut events = native(&t);
        settle(&mut events, &t).await;
        std::fs::remove_dir_all(&t.auth_dir).unwrap();
        std::fs::create_dir(&t.auth_dir).unwrap();
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
    #[tokio::test]
    async fn survives_the_auth_folder_going_away() {
        for rename in [true, false] {
            let root = temp();
            let t = Targets {
                config: root.join("config.yaml"),
                auth_dir: root.join("auth"),
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
            std::fs::create_dir(&t.auth_dir).unwrap();
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
    #[test]
    fn dangling_auth_links_watch_for_their_target() {
        let root = temp();
        std::os::unix::fs::symlink(root.join("later/x.json"), root.join("auth/x.json")).unwrap();
        let t = Targets {
            config: root.join("auth/config.yaml"),
            auth_dir: root.join("auth"),
        };
        let d = dirs(&t);
        let top = d.iter().find(|d| d.path == root).expect("target's ancestor watched");
        assert!(top.matters("later".as_ref()) && !top.content, "{d:?}");
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A config bind-mounted as a single file: the write goes through another path to
    /// the same inode and notifies only the file (a hard link stands in for the mount).
    #[tokio::test]
    async fn sees_writes_to_the_config_through_another_path() {
        let root = temp();
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::create_dir_all(root.join("b")).unwrap();
        let t = Targets {
            config: root.join("a/config.yaml"),
            auth_dir: root.join("auth"),
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
