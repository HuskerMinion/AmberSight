//! Files in a copy folder (event_copy.rs) that other people may write in too
//! (a shared or synced folder, a drive where every user may change things).
//! Nothing is looked up by path again after it was checked: every folder is
//! opened once and everything inside it is done relative to that open folder.
//!
//! - Unix: each level is opened with `openat(O_DIRECTORY | O_NOFOLLOW)` from
//!   the folder above; files are made with `O_CREAT | O_EXCL | O_NOFOLLOW`,
//!   renamed with `renameat` and deleted with `unlinkat`, all relative to the
//!   folder's descriptor. Running as root, a camera or date folder must belong
//!   to root or to the copy folder's owner; new files and folders are given to
//!   the copy folder's owner.
//! - Windows: each level is opened as a handle that does not follow links or
//!   junctions (a link or junction is refused; OneDrive's own cloud folders
//!   are fine) and that others may not delete or rename while it is open, so
//!   the path to it cannot change underneath. Every file made or opened is
//!   checked to really be in that folder (its final path), and is renamed and
//!   deleted through its own handle, never by path.

use std::fs::File;
use std::io;
use std::path::Path;
#[cfg(windows)]
use std::path::PathBuf;
use std::time::SystemTime;

/// What a folder entry is. A link (or junction) is `Other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Other,
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub kind: Kind,
    pub modified: Option<SystemTime>,
    pub len: u64,
}

/// A file just made in a [`Dir`] (under a name nobody can guess).
pub struct NewFile {
    pub file: File,
    #[cfg_attr(windows, allow(dead_code))]
    name: String,
}

/// Longest name a folder entry may have (UTF-16 units on Windows, bytes on most Unix file
/// systems; checked as the larger of the two).
const NAME_UNITS_MAX: usize = 255;

fn bad_name(name: &str) -> bool {
    name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\', '\0', ':']) || name.encode_utf16().count() > NAME_UNITS_MAX
}

fn refuse(name: &str, what: &str) -> io::Error {
    io::Error::other(format!("\"{name}\" there is {what}"))
}

/// Where `path` really is, resolved once: the path the checks look at and
/// the one [`Dir::open_root`] opens.
pub fn canonical(path: &Path) -> io::Result<std::path::PathBuf> {
    std::fs::canonicalize(path)
}

// ------------------------------------------------------------------ Unix

#[cfg(unix)]
pub struct Dir {
    fd: std::os::fd::OwnedFd,
    /// The copy folder's owner and permission bits (new things follow them).
    root_uid: u32,
    root_gid: u32,
    root_mode: u32,
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::ffi::{CStr, CString};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    fn c(name: &str) -> io::Result<CString> {
        if bad_name(name) {
            return Err(io::Error::other("not a plain name"));
        }
        CString::new(name).map_err(|_| io::Error::other("not a plain name"))
    }

    fn fstat(fd: i32) -> io::Result<libc::stat> {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: fd is open; st is a valid out pointer.
        if unsafe { libc::fstat(fd, &mut st) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(st)
    }

    fn mtime(st: &libc::stat) -> Option<SystemTime> {
        let s = st.st_mtime;
        (s >= 0).then(|| SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(s as u64))
    }

    fn is_root() -> bool {
        // SAFETY: no preconditions.
        unsafe { libc::geteuid() == 0 }
    }

    impl Dir {
        /// The copy folder at `real`, a path already resolved (see
        /// [`canonical`]) and checked: opened one folder at a time from `/`
        /// without following any link, so a link put anywhere on the way
        /// since the check makes it fail instead of leading elsewhere.
        pub fn open_root(real: &Path) -> io::Result<Dir> {
            if !real.is_absolute() {
                return Err(io::Error::other("not a full path"));
            }
            let slash = CString::new("/").unwrap();
            // SAFETY: a valid C string.
            let fd = unsafe { libc::open(slash.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: fd was just opened and is owned here.
            let mut fd = unsafe { OwnedFd::from_raw_fd(fd) };
            for part in real.components().skip(1) {
                let std::path::Component::Normal(name) = part else { return Err(io::Error::other("not a plain path")) };
                let cn = CString::new(name.as_encoded_bytes()).map_err(|_| io::Error::other("bad path"))?;
                // SAFETY: fd is open; cn is a valid C string.
                let next = unsafe { libc::openat(fd.as_raw_fd(), cn.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
                if next < 0 {
                    let e = io::Error::last_os_error();
                    return Err(if matches!(e.raw_os_error(), Some(libc::ELOOP) | Some(libc::ENOTDIR)) { io::Error::other("the copy folder's path changed (a link appeared on the way)") } else { e });
                }
                // SAFETY: as above.
                fd = unsafe { OwnedFd::from_raw_fd(next) };
            }
            let st = fstat(fd.as_raw_fd())?;
            Ok(Dir { fd, root_uid: st.st_uid, root_gid: st.st_gid, root_mode: st.st_mode as u32 & 0o7777 })
        }

        /// What this folder is, whatever its path: (device, inode).
        pub fn identity(&self) -> io::Result<(u64, u64)> {
            let st = fstat(self.fd.as_raw_fd())?;
            Ok((st.st_dev as u64, st.st_ino as u64))
        }

        /// Anyone on this computer may change things in the copy folder.
        pub fn world_writable(&self) -> bool {
            self.root_mode & 0o002 != 0
        }

        fn dir_mode(&self) -> u32 {
            (self.root_mode & 0o770) | 0o700
        }

        fn file_mode(&self) -> u32 {
            (self.root_mode & 0o660) | 0o600
        }

        /// The folder `name` in this one (made when `create` and missing). A
        /// link, a file, or (as root) a folder of another account is refused.
        pub fn sub(&self, name: &str, create: bool) -> io::Result<Dir> {
            let cn = c(name)?;
            let open = || {
                // SAFETY: self.fd is open; cn is a valid C string.
                let fd = unsafe { libc::openat(self.fd.as_raw_fd(), cn.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
                if fd < 0 { Err(io::Error::last_os_error()) } else { Ok(unsafe { OwnedFd::from_raw_fd(fd) }) }
            };
            let mut made = false;
            let fd = match open() {
                Ok(fd) => fd,
                Err(e) if e.kind() == io::ErrorKind::NotFound && create => {
                    // SAFETY: as above.
                    if unsafe { libc::mkdirat(self.fd.as_raw_fd(), cn.as_ptr(), self.dir_mode() as libc::mode_t) } != 0 {
                        let e = io::Error::last_os_error();
                        if e.kind() != io::ErrorKind::AlreadyExists {
                            return Err(e);
                        }
                    } else {
                        made = true;
                    }
                    open()?
                }
                Err(e) if matches!(e.raw_os_error(), Some(libc::ELOOP) | Some(libc::ENOTDIR)) => return Err(refuse(name, "a link or a file, not a folder (AmberSight does not write through links)")),
                Err(e) => return Err(e),
            };
            let st = fstat(fd.as_raw_fd())?;
            let d = Dir { fd, root_uid: self.root_uid, root_gid: self.root_gid, root_mode: self.root_mode };
            if is_root() {
                // Given to the copy folder's owner only when it is the very
                // folder just made: ours and empty (not one renamed into place).
                // SAFETY: no preconditions.
                let me = unsafe { libc::geteuid() };
                if made && self.root_uid != 0 && st.st_uid == me && d.entries()?.is_empty() {
                    // SAFETY: fd is open.
                    unsafe { libc::fchown(d.fd.as_raw_fd(), self.root_uid, self.root_gid) };
                } else if st.st_uid != 0 && st.st_uid != self.root_uid {
                    return Err(refuse(name, "a folder of another account"));
                }
            }
            Ok(d)
        }

        /// A new file `name` (refused if anything is there, a link too).
        pub fn create_new(&self, name: &str) -> io::Result<NewFile> {
            let cn = c(name)?;
            // SAFETY: self.fd is open; cn is a valid C string.
            let fd = unsafe { libc::openat(self.fd.as_raw_fd(), cn.as_ptr(), libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC, self.file_mode() as libc::c_uint) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: fd was just opened and is owned here.
            let file = unsafe { File::from_raw_fd(fd) };
            if is_root() && self.root_uid != 0 {
                // SAFETY: fd is open.
                unsafe { libc::fchown(file.as_raw_fd(), self.root_uid, self.root_gid) };
            }
            Ok(NewFile { file, name: name.to_string() })
        }

        /// Give a file made here its final name (replacing what is there).
        pub fn rename_into(&self, f: &NewFile, to: &str) -> io::Result<()> {
            let (a, b) = (c(&f.name)?, c(to)?);
            // SAFETY: both names are valid C strings; the fds are open.
            if unsafe { libc::renameat(self.fd.as_raw_fd(), a.as_ptr(), self.fd.as_raw_fd(), b.as_ptr()) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        /// Throw away a file made here.
        pub fn discard(&self, f: NewFile) {
            if let Ok(n) = c(&f.name) {
                // SAFETY: as above.
                unsafe { libc::unlinkat(self.fd.as_raw_fd(), n.as_ptr(), 0) };
            }
        }

        fn stat_at(&self, name: &str) -> io::Result<libc::stat> {
            let cn = c(name)?;
            let mut st: libc::stat = unsafe { std::mem::zeroed() };
            // SAFETY: as above.
            if unsafe { libc::fstatat(self.fd.as_raw_fd(), cn.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(st)
        }

        /// Delete the plain file `name` (not a link, not a folder).
        pub fn remove_file(&self, name: &str) -> io::Result<()> {
            let st = self.stat_at(name)?;
            if st.st_mode & libc::S_IFMT != libc::S_IFREG {
                return Err(refuse(name, "not a plain file"));
            }
            let cn = c(name)?;
            // SAFETY: as above.
            if unsafe { libc::unlinkat(self.fd.as_raw_fd(), cn.as_ptr(), 0) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        /// Delete the folder `name` if it is empty (a link is not removed).
        pub fn remove_empty_dir(&self, name: &str) -> io::Result<()> {
            let cn = c(name)?;
            // SAFETY: as above.
            if unsafe { libc::unlinkat(self.fd.as_raw_fd(), cn.as_ptr(), libc::AT_REMOVEDIR) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        /// What is in this folder (links are not followed).
        pub fn entries(&self) -> io::Result<Vec<Entry>> {
            // SAFETY: dup of an open fd; fdopendir takes ownership of the copy.
            let dupfd = unsafe { libc::fcntl(self.fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
            if dupfd < 0 {
                return Err(io::Error::last_os_error());
            }
            let dir = unsafe { libc::fdopendir(dupfd) };
            if dir.is_null() {
                let e = io::Error::last_os_error();
                unsafe { libc::close(dupfd) };
                return Err(e);
            }
            unsafe { libc::rewinddir(dir) };
            let mut out = Vec::new();
            loop {
                // SAFETY: dir is a valid DIR*.
                let ent = unsafe { libc::readdir(dir) };
                if ent.is_null() {
                    break;
                }
                // SAFETY: d_name is NUL-terminated.
                let name = unsafe { CStr::from_ptr((*ent).d_name.as_ptr()) }.to_string_lossy().into_owned();
                if name == "." || name == ".." || bad_name(&name) {
                    continue;
                }
                let Ok(st) = self.stat_at(&name) else { continue };
                let kind = match st.st_mode & libc::S_IFMT {
                    libc::S_IFREG => Kind::File,
                    libc::S_IFDIR => Kind::Dir,
                    _ => Kind::Other,
                };
                out.push(Entry { name, kind, modified: mtime(&st), len: st.st_size as u64 });
            }
            // SAFETY: dir is valid; this closes dupfd.
            unsafe { libc::closedir(dir) };
            Ok(out)
        }

        /// (free, total) bytes on this folder's drive.
        pub fn space(&self) -> io::Result<(u64, u64)> {
            let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
            // SAFETY: fd is open; s is a valid out pointer.
            if unsafe { libc::fstatvfs(self.fd.as_raw_fd(), &mut s) } != 0 {
                return Err(io::Error::last_os_error());
            }
            let f = s.f_frsize as u64;
            Ok((s.f_bavail as u64 * f, s.f_blocks as u64 * f))
        }
    }
}

// --------------------------------------------------------------- Windows

#[cfg(windows)]
pub struct Dir {
    handle: File,
    /// Where it really is (`\\?\C:\...` or `\\?\UNC\server\share\...`).
    path: PathBuf,
}

#[cfg(windows)]
mod win {
    use super::*;
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use windows_sys::Win32::Storage::FileSystem::*;

    const SHARE_NO_DELETE: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE;
    const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
    const ERROR_SHARING_VIOLATION: i32 = 32;
    const ERROR_ACCESS_DENIED: i32 = 5;

    // The native calls: opening and renaming relative to a folder handle,
    // with no path looked up at all.
    #[repr(C)]
    struct UnicodeString {
        len: u16,
        max: u16,
        buf: *const u16,
    }
    #[repr(C)]
    struct ObjectAttributes {
        len: u32,
        root: isize,
        name: *const UnicodeString,
        attrs: u32,
        sd: *const std::ffi::c_void,
        qos: *const std::ffi::c_void,
    }
    #[repr(C)]
    struct IoStatusBlock {
        status: usize,
        info: usize,
    }
    #[link(name = "ntdll")]
    extern "system" {
        fn NtCreateFile(h: *mut isize, access: u32, oa: *const ObjectAttributes, iosb: *mut IoStatusBlock, alloc: *const i64, attrs: u32, share: u32, disposition: u32, options: u32, ea: *const std::ffi::c_void, ea_len: u32) -> i32;
        fn NtSetInformationFile(h: isize, iosb: *mut IoStatusBlock, info: *const std::ffi::c_void, len: u32, class: u32) -> i32;
        fn RtlNtStatusToDosError(status: i32) -> u32;
    }
    const OBJ_CASE_INSENSITIVE: u32 = 0x40;
    const FILE_OPEN: u32 = 1;
    const FILE_CREATE: u32 = 2;
    const FILE_DIRECTORY_FILE: u32 = 0x1;
    const FILE_SYNCHRONOUS_IO_NONALERT: u32 = 0x20;
    const FILE_NON_DIRECTORY_FILE: u32 = 0x40;
    const FILE_OPEN_FOR_BACKUP_INTENT: u32 = 0x4000;
    const FILE_OPEN_REPARSE_POINT_OPT: u32 = 0x0020_0000;
    const FILE_RENAME_INFORMATION_CLASS: u32 = 10;

    fn nt_err(status: i32) -> io::Error {
        // SAFETY: no preconditions.
        io::Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(status) } as i32)
    }

    /// Open (or make) `name` in the folder `dir` by its handle. The name is
    /// one plain name, so nothing on the way can be swapped; the thing itself
    /// is opened, not what a link there points to.
    fn nt_open(dir: &File, name: &str, access: u32, share: u32, disposition: u32, options: u32) -> io::Result<File> {
        let w: Vec<u16> = name.encode_utf16().collect();
        // (bad_name refuses these already; the length below must fit in a u16.)
        if w.len() > super::NAME_UNITS_MAX {
            return Err(io::Error::other("the name is too long"));
        }
        let us = UnicodeString { len: (w.len() * 2) as u16, max: (w.len() * 2) as u16, buf: w.as_ptr() };
        let oa = ObjectAttributes { len: std::mem::size_of::<ObjectAttributes>() as u32, root: dir.as_raw_handle() as isize, name: &us, attrs: OBJ_CASE_INSENSITIVE, sd: std::ptr::null(), qos: std::ptr::null() };
        let mut h: isize = 0;
        let mut io = IoStatusBlock { status: 0, info: 0 };
        // SAFETY: every pointer is valid for the call; dir is an open folder handle.
        let st = unsafe { NtCreateFile(&mut h, access | SYNCHRONIZE, &oa, &mut io, std::ptr::null(), 0, share, disposition, options | FILE_SYNCHRONOUS_IO_NONALERT | FILE_OPEN_REPARSE_POINT_OPT, std::ptr::null(), 0) };
        if st < 0 {
            return Err(nt_err(st));
        }
        // SAFETY: h was just opened and is owned here.
        Ok(unsafe { File::from_raw_handle(h as _) })
    }

    pub(super) fn final_path(f: &File) -> io::Result<PathBuf> {
        let mut buf = vec![0u16; 1024];
        loop {
            // SAFETY: the handle is open; buf is writable for its length.
            let n = unsafe { GetFinalPathNameByHandleW(f.as_raw_handle() as _, buf.as_mut_ptr(), buf.len() as u32, FILE_NAME_NORMALIZED) } as usize;
            if n == 0 {
                return Err(io::Error::last_os_error());
            }
            if n < buf.len() {
                return Ok(PathBuf::from(String::from_utf16_lossy(&buf[..n])));
            }
            buf.resize(n + 1, 0);
        }
    }

    /// The reparse tag of what `f` is, when it is a reparse point.
    fn reparse_tag(f: &File) -> io::Result<Option<u32>> {
        let mut info = FILE_ATTRIBUTE_TAG_INFO { FileAttributes: 0, ReparseTag: 0 };
        // SAFETY: the handle is open; info is a valid out buffer of its size.
        let ok = unsafe { GetFileInformationByHandleEx(f.as_raw_handle() as _, FileAttributeTagInfo, &mut info as *mut _ as *mut _, std::mem::size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0).then_some(info.ReparseTag))
    }

    /// OneDrive's (cloud files) tags: IO_REPARSE_TAG_CLOUD and CLOUD_1..F.
    fn cloud_tag(tag: u32) -> bool {
        tag & 0xFFFF_0FFF == 0x9000_001A
    }

    /// A reparse point AmberSight may use: cloud files for folders; for
    /// files also deduplicated and compressed (WOF) ones. Everything else
    /// (links, junctions, mount points, unknown kinds) is refused.
    fn tag_ok(tag: Option<u32>, dir: bool) -> bool {
        match tag {
            None => true,
            Some(t) => cloud_tag(t) || (!dir && matches!(t, 0x8000_0013 | 0x8000_0017)),
        }
    }

    fn same_path(a: &Path, b: &Path) -> bool {
        a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
    }

    fn set_info(f: &File, class: FILE_INFO_BY_HANDLE_CLASS, buf: *const std::ffi::c_void, len: usize) -> io::Result<()> {
        // SAFETY: the handle is open; buf points to len readable bytes.
        if unsafe { SetFileInformationByHandle(f.as_raw_handle() as _, class, buf, len as u32) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn delete_by_handle(f: &File) -> io::Result<()> {
        let info = FILE_DISPOSITION_INFO { DeleteFile: 1 };
        set_info(f, FileDispositionInfo, &info as *const _ as *const _, std::mem::size_of::<FILE_DISPOSITION_INFO>())
    }

    /// Rename through the file's handle to the plain name `to` in the folder
    /// `dir`, named relative to the folder's handle (no path looked up).
    fn rename_by_handle(f: &File, dir: &File, to: &str) -> io::Result<()> {
        let name: Vec<u16> = to.encode_utf16().collect();
        let head = std::mem::offset_of!(FILE_RENAME_INFO, FileName);
        let len = head + (name.len() + 1) * 2;
        let mut buf = vec![0u64; len.div_ceil(8)];
        // SAFETY: buf is at least len bytes and 8-aligned, like the struct
        // (FILE_RENAME_INFORMATION has the same layout).
        unsafe {
            let p = buf.as_mut_ptr() as *mut FILE_RENAME_INFO;
            (*p).Anonymous.ReplaceIfExists = 1;
            (*p).RootDirectory = dir.as_raw_handle() as _;
            (*p).FileNameLength = (name.len() * 2) as u32;
            std::ptr::copy_nonoverlapping(name.as_ptr(), (*p).FileName.as_mut_ptr(), name.len());
        }
        let mut io = IoStatusBlock { status: 0, info: 0 };
        // SAFETY: the handle is open; buf holds len bytes.
        let st = unsafe { NtSetInformationFile(f.as_raw_handle() as isize, &mut io, buf.as_ptr() as *const _, len as u32, FILE_RENAME_INFORMATION_CLASS) };
        if st < 0 {
            return Err(nt_err(st));
        }
        Ok(())
    }

    fn file_id(f: &File) -> io::Result<(u64, u64)> {
        let mut i: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: the handle is open; i is a valid out pointer.
        if unsafe { GetFileInformationByHandle(f.as_raw_handle() as _, &mut i) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((i.dwVolumeSerialNumber as u64, ((i.nFileIndexHigh as u64) << 32) | i.nFileIndexLow as u64))
    }

    impl Dir {
        /// The copy folder at `real`, a path already resolved (see
        /// [`canonical`]) and checked. Opened without following a link; it
        /// must be a folder (OneDrive's cloud folders are fine, links,
        /// junctions and mount points are not) and still be at that path.
        pub fn open_root(real: &Path) -> io::Result<Dir> {
            let handle = std::fs::OpenOptions::new()
                .access_mode(FILE_LIST_DIRECTORY | FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE)
                .share_mode(SHARE_NO_DELETE)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                .open(real)?;
            if handle.metadata()?.file_attributes() & FILE_ATTRIBUTE_DIRECTORY == 0 {
                return Err(io::Error::other("not a folder"));
            }
            if !tag_ok(reparse_tag(&handle)?, true) {
                return Err(io::Error::other("the copy folder is a link, junction or mount point"));
            }
            let path = final_path(&handle)?;
            if !same_path(&path, real) {
                return Err(io::Error::other("the copy folder's path changed (a link appeared on the way)"));
            }
            Ok(Dir { handle, path })
        }

        /// What this folder is, whatever its path: (volume serial, file id).
        pub fn identity(&self) -> io::Result<(u64, u64)> {
            file_id(&self.handle)
        }

        pub fn world_writable(&self) -> bool {
            false
        }

        fn check(name: &str) -> io::Result<()> {
            if bad_name(name) {
                return Err(io::Error::other("not a plain name"));
            }
            Ok(())
        }

        pub fn sub(&self, name: &str, create: bool) -> io::Result<Dir> {
            Dir::check(name)?;
            let access = FILE_LIST_DIRECTORY | FILE_TRAVERSE | FILE_ADD_FILE | FILE_ADD_SUBDIRECTORY | FILE_READ_ATTRIBUTES;
            let open = |disp| nt_open(&self.handle, name, access, SHARE_NO_DELETE, disp, FILE_DIRECTORY_FILE | FILE_OPEN_FOR_BACKUP_INTENT);
            let handle = match open(FILE_OPEN) {
                Ok(h) => h,
                Err(e) if e.kind() == io::ErrorKind::NotFound && create => match open(FILE_CREATE) {
                    Ok(h) => h,
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => open(FILE_OPEN)?,
                    Err(e) => return Err(e),
                },
                // A file there (opened as a folder).
                Err(e) if e.raw_os_error() == Some(267) => return Err(refuse(name, "a file, not a folder")),
                Err(e) => return Err(e),
            };
            if !tag_ok(reparse_tag(&handle)?, true) {
                return Err(refuse(name, "a link or junction, not a folder (AmberSight does not write through links)"));
            }
            let path = final_path(&handle)?;
            if !same_path(&path, &self.path.join(name)) {
                return Err(refuse(name, "not where it should be"));
            }
            Ok(Dir { handle, path })
        }

        pub fn create_new(&self, name: &str) -> io::Result<NewFile> {
            Dir::check(name)?;
            let file = nt_open(&self.handle, name, FILE_GENERIC_WRITE | DELETE, FILE_SHARE_READ, FILE_CREATE, FILE_NON_DIRECTORY_FILE)?;
            Ok(NewFile { file, name: name.to_string() })
        }

        pub fn rename_into(&self, f: &NewFile, to: &str) -> io::Result<()> {
            Dir::check(to)?;
            // A sync app or a virus scanner may have the old copy open a moment.
            let mut tries = 0;
            loop {
                match rename_by_handle(&f.file, &self.handle, to) {
                    Err(e) if tries < 5 && matches!(e.raw_os_error(), Some(ERROR_SHARING_VIOLATION) | Some(ERROR_ACCESS_DENIED)) => {
                        tries += 1;
                        std::thread::sleep(std::time::Duration::from_millis(300));
                    }
                    r => return r,
                }
            }
        }

        pub fn discard(&self, f: NewFile) {
            let _ = delete_by_handle(&f.file);
        }

        /// Open `name` here for deleting, checked to be what it should be.
        fn open_for_delete(&self, name: &str, dir: bool) -> io::Result<File> {
            Dir::check(name)?;
            let kind = if dir { FILE_DIRECTORY_FILE | FILE_OPEN_FOR_BACKUP_INTENT } else { FILE_NON_DIRECTORY_FILE };
            let f = nt_open(&self.handle, name, DELETE | FILE_READ_ATTRIBUTES, SHARE_ALL, FILE_OPEN, kind)?;
            if !tag_ok(reparse_tag(&f)?, dir) {
                return Err(refuse(name, if dir { "not a plain folder" } else { "not a plain file" }));
            }
            Ok(f)
        }

        pub fn remove_file(&self, name: &str) -> io::Result<()> {
            delete_by_handle(&self.open_for_delete(name, false)?)
        }

        pub fn remove_empty_dir(&self, name: &str) -> io::Result<()> {
            delete_by_handle(&self.open_for_delete(name, true)?)
        }

        pub fn entries(&self) -> io::Result<Vec<Entry>> {
            let pat: Vec<u16> = self.path.join("*").as_os_str().to_string_lossy().encode_utf16().chain(std::iter::once(0)).collect();
            let mut d: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
            // SAFETY: pat is NUL-terminated; d is a valid out pointer.
            let h = unsafe { FindFirstFileW(pat.as_ptr(), &mut d) };
            if h == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }
            let mut out = Vec::new();
            loop {
                let end = d.cFileName.iter().position(|&c| c == 0).unwrap_or(d.cFileName.len());
                let name = String::from_utf16_lossy(&d.cFileName[..end]);
                if !(name == "." || name == ".." || bad_name(&name)) {
                    let is_dir = d.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
                    let tag = (d.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0).then_some(d.dwReserved0);
                    let kind = if !tag_ok(tag, is_dir) { Kind::Other } else if is_dir { Kind::Dir } else { Kind::File };
                    let ft = ((d.ftLastWriteTime.dwHighDateTime as u64) << 32) | d.ftLastWriteTime.dwLowDateTime as u64;
                    // 100 ns steps since 1601.
                    let modified = ft.checked_sub(116_444_736_000_000_000).map(|t| SystemTime::UNIX_EPOCH + std::time::Duration::from_nanos(t * 100));
                    out.push(Entry { name, kind, modified, len: ((d.nFileSizeHigh as u64) << 32) | d.nFileSizeLow as u64 });
                }
                // SAFETY: h is a valid find handle.
                if unsafe { FindNextFileW(h, &mut d) } == 0 {
                    break;
                }
            }
            // SAFETY: as above.
            unsafe { FindClose(h) };
            Ok(out)
        }

        pub fn space(&self) -> io::Result<(u64, u64)> {
            Ok((fs4::available_space(&self.path)?, fs4::total_space(&self.path)?))
        }
    }

    #[cfg(test)]
    #[test]
    fn reparse_tags_allowed() {
        for (t, dir, ok) in [(0x9000_001A, true, true), (0x9000_F01A, true, true), (0x9000_701A, false, true), (0xA000_000C, false, false), (0xA000_0003, true, false), (0x8000_0013, false, true), (0x8000_0013, true, false), (0x8000_0017, false, true), (0x8000_0014, false, false)] {
            assert_eq!(tag_ok(Some(t), dir), ok, "{t:x}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn names_too_long_are_refused() {
        assert!(!bad_name(&"a".repeat(255)));
        assert!(bad_name(&"a".repeat(256)));
        // 32768 units would wrap to 0 in the u16 byte length Windows takes.
        assert!(bad_name(&"\u{1F600}".repeat(16384)));
        assert!(bad_name(&"\u{1F600}".repeat(128)), "two UTF-16 units each");
    }

    #[test]
    fn made_renamed_listed_and_deleted_inside_the_folder_only() {
        let base = std::env::current_dir().unwrap().join("target").join(format!("copy-fs-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("root")).unwrap();
        std::fs::create_dir_all(base.join("outside")).unwrap();
        std::fs::write(base.join("outside").join("victim.mp4"), b"keep").unwrap();
        let root = Dir::open_root(&canonical(&base.join("root")).unwrap()).unwrap();
        let cam = root.sub("Cam", true).unwrap();
        let day = cam.sub("2026-11-13", true).unwrap();
        let mut f = day.create_new("a.part").unwrap();
        f.file.write_all(b"video").unwrap();
        assert!(day.create_new("a.part").is_err(), "never over something there");
        day.rename_into(&f, "10-00-00.mp4").unwrap();
        drop(f);
        assert_eq!(std::fs::read(base.join("root/Cam/2026-11-13/10-00-00.mp4")).unwrap(), b"video");
        let names: Vec<(String, Kind, u64)> = day.entries().unwrap().into_iter().map(|e| (e.name, e.kind, e.len)).collect();
        assert_eq!(names, vec![("10-00-00.mp4".to_string(), Kind::File, 5)]);
        let g = day.create_new("b.part").unwrap();
        day.discard(g);
        assert_eq!(day.entries().unwrap().len(), 1);
        assert!(cam.remove_empty_dir("2026-11-13").is_err(), "not empty");
        day.remove_file("10-00-00.mp4").unwrap();
        drop(day);
        cam.remove_empty_dir("2026-11-13").unwrap();
        assert!(cam.sub("..", true).is_err() && cam.create_new("a/b").is_err());
        assert!(root.space().unwrap().1 > 0);
        #[cfg(unix)]
        {
            // A link where a folder should be: never followed, never deleted through.
            std::os::unix::fs::symlink(base.join("outside"), base.join("root/Cam/2020-01-01")).unwrap();
            assert!(cam.sub("2020-01-01", true).is_err());
            assert!(cam.remove_file("2020-01-01").is_err());
            assert!(cam.entries().unwrap().iter().any(|e| e.name == "2020-01-01" && e.kind == Kind::Other));
            // The folder swapped for a link after it was opened: still the real one.
            let day = cam.sub("2026-11-14", true).unwrap();
            std::fs::rename(base.join("root/Cam/2026-11-14"), base.join("root/moved")).unwrap();
            std::os::unix::fs::symlink(base.join("outside"), base.join("root/Cam/2026-11-14")).unwrap();
            let f = day.create_new("x.part").unwrap();
            day.rename_into(&f, "victim.mp4").unwrap();
            assert_eq!(std::fs::read(base.join("outside/victim.mp4")).unwrap(), b"keep");
            assert!(base.join("root/moved/victim.mp4").exists());
        }
        #[cfg(windows)]
        {
            // A junction where a folder should be: refused, listed as neither.
            let j = base.join("root").join("Cam").join("2020-01-01");
            let made = std::process::Command::new("cmd").args(["/c", "mklink", "/J"]).arg(&j).arg(base.join("outside")).output().unwrap();
            assert!(made.status.success(), "{made:?}");
            assert!(cam.sub("2020-01-01", true).is_err());
            assert!(cam.remove_file("2020-01-01").is_err() && cam.remove_empty_dir("2020-01-01").is_err());
            assert!(cam.entries().unwrap().iter().any(|e| e.name == "2020-01-01" && e.kind == Kind::Other));
            // While a folder is open here it cannot be moved away and swapped
            // for a junction.
            let day = cam.sub("2026-11-14", true).unwrap();
            assert!(std::fs::rename(base.join("root").join("Cam").join("2026-11-14"), base.join("root").join("moved")).is_err());
            let f = day.create_new("x.part").unwrap();
            day.rename_into(&f, "victim.mp4").unwrap();
            drop((f, day));
            assert_eq!(std::fs::read(base.join("outside").join("victim.mp4")).unwrap(), b"keep");
            assert!(base.join("root").join("Cam").join("2026-11-14").join("victim.mp4").exists());
            std::fs::remove_dir(&j).unwrap();
        }
        drop((cam, root));
        let _ = std::fs::remove_dir_all(&base);
    }
}
