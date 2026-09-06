//! Remote file operations use explicit paths and the server process's Unix permissions.
//! Interface uploads publish through an opened directory; desktop drops and pastes stage in
//! cache batches until their Wayland recipient chooses a destination. Unclaimed batches are
//! linked or copied to the transfer folder, and staged sources remain for the cache sweep.

use std::{
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use axum::body::Body;
use futures_util::StreamExt;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{fs::{File, OpenOptions}, os::{fd::AsRawFd, unix::fs::OpenOptionsExt}};
use tokio::io::AsyncWriteExt;

use crate::{App, api::ApiError};

/// One file in the folder.
#[derive(Debug, Serialize, JsonSchema)]
pub struct FileInfo {
    pub name: String,
    pub size: u64,
    /// last modification, ms since the epoch
    pub modified_ms: u64,
}

/// Browser paths are absolute UTF-8 paths or the exact @home / @transfer shortcuts.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FileQuery {
    pub path: Option<String>,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub sort: FileSort,
    #[serde(default)]
    pub desc: bool,
    #[serde(default)]
    pub offset: usize,
    pub limit: Option<usize>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FileSort { #[default] Name, Size, Modified }

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind { Directory, File, Symlink, Other }

#[derive(Debug, Serialize, JsonSchema)]
pub struct FileEntry {
    pub name: String,
    pub kind: EntryKind,
    pub target_kind: Option<EntryKind>,
    pub size: u64,
    pub modified_ms: u64,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct FileListing {
    pub path: String,
    pub entries: Vec<FileEntry>,
    pub total: usize,
    pub offset: usize,
    pub limit: usize,
    pub omitted: usize,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SavedFile { pub name: String, pub path: String, pub directory: String }

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum FileAction {
    Mkdir { path: String, name: String },
    Rename { path: String, name: String, new_name: String },
}

fn file_error(code: &'static str, message: impl Into<String>) -> ApiError {
    ApiError::File { code, message: message.into() }
}

fn io_error(e: std::io::Error) -> ApiError {
    let code = match e.raw_os_error() {
        Some(libc::ENOENT) => "missing",
        Some(libc::EACCES | libc::EPERM) => "permission_denied",
        Some(libc::EEXIST) => "exists",
        Some(libc::ENOTDIR) => "not_directory",
        Some(libc::EISDIR) => "is_directory",
        Some(libc::EINVAL | libc::ENAMETOOLONG | libc::ELOOP) => "invalid_path",
        _ => "filesystem",
    };
    file_error(code, e.to_string())
}

fn entry_name(name: &str) -> Result<&str, ApiError> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
        Err(file_error("invalid_path", "expected one entry name"))
    } else { Ok(name) }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, ApiError> + Send + 'static) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(f).await.map_err(|e| ApiError::Internal(e.to_string()))?
}

struct Directory(File);
impl Directory {
    fn open(path: &Path) -> Result<Self, ApiError> {
        let file = OpenOptions::new().read(true).custom_flags(libc::O_PATH | libc::O_CLOEXEC).open(path).map_err(io_error)?;
        if !file.metadata().map_err(io_error)?.is_dir() { return Err(file_error("not_directory", "not a directory")); }
        Ok(Self(file))
    }
    fn at(&self, name: &str) -> PathBuf {
        let path = PathBuf::from(format!("/proc/self/fd/{}", self.0.as_raw_fd()));
        if name.is_empty() { path } else { path.join(name) }
    }
    fn path(&self) -> Result<String, ApiError> {
        if self.0.metadata().map_err(io_error)?.nlink() == 0 { return Err(file_error("missing", "destination directory was removed")); }
        std::fs::read_link(self.at("")).map_err(io_error)?.into_os_string().into_string().map_err(|_| file_error("invalid_path", "path is not UTF-8"))
    }
    fn saved(&self, name: String) -> Result<SavedFile, ApiError> {
        let directory = self.path()?;
        let path = Path::new(&directory).join(&name).to_str().unwrap().to_owned();
        Ok(SavedFile { name, path, directory })
    }
}
use std::os::unix::fs::MetadataExt;

/// Inspect an O_PATH descriptor before opening the same inode for data. Special files are never activated.
fn regular_file(path: &Path, follow: bool) -> Result<File, ApiError> {
    let fd = OpenOptions::new().read(true).custom_flags(libc::O_PATH | libc::O_CLOEXEC | if follow { 0 } else { libc::O_NOFOLLOW }).open(path).map_err(io_error)?;
    if !fd.metadata().map_err(io_error)?.is_file() { return Err(file_error("unsupported_type", "only regular files can be downloaded")); }
    OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC).open(format!("/proc/self/fd/{}", fd.as_raw_fd())).map_err(io_error)
}

fn file_body(file: File) -> Result<(Option<u64>, Body), ApiError> {
    let size = file.metadata().map_err(io_error)?.len();
    let mut fs = std::mem::MaybeUninit::<libc::statfs>::uninit();
    let virtual_fs = unsafe { libc::fstatfs(file.as_raw_fd(), fs.as_mut_ptr()) } == 0 && {
        let fs = unsafe { fs.assume_init() };
        fs.f_type == libc::PROC_SUPER_MAGIC || fs.f_type == libc::SYSFS_MAGIC
    };
    // procfs/sysfs report synthetic lengths. Zero-size regular files may also generate data on read.
    let len = if size == 0 || virtual_fs { None } else { Some(size) };
    Ok((len, Body::from_stream(tokio_util::io::ReaderStream::new(tokio::io::AsyncReadExt::take(tokio::fs::File::from_std(file), len.unwrap_or(u64::MAX))))))
}

fn link_file(dir: &Directory, file: &File, name: &str) -> std::io::Result<String> {
    let source = std::ffi::CString::new(format!("/proc/self/fd/{}", file.as_raw_fd())).unwrap();
    for candidate in candidates(name) {
        let target = std::ffi::CString::new(candidate.as_str()).unwrap();
        // Publish the opened inode even if its original directory entry has been replaced.
        if unsafe { libc::linkat(libc::AT_FDCWD, source.as_ptr(), dir.0.as_raw_fd(), target.as_ptr(), libc::AT_SYMLINK_FOLLOW) } == 0 { return Ok(candidate); }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::AlreadyExists { return Err(error); }
    }
    unreachable!()
}

fn publish_error(error: std::io::Error) -> ApiError {
    if matches!(error.raw_os_error(), Some(libc::EPERM | libc::EOPNOTSUPP | libc::ENOSYS)) {
        file_error("unsupported_operation", "this filesystem or its permissions prevent publishing the file without replacement")
    } else { io_error(error) }
}

struct PartialUpload { dir: Directory, part: String, file: Option<File>, identity: (u64, u64) }
impl Drop for PartialUpload {
    fn drop(&mut self) {
        let path = self.dir.at(&self.part);
        if std::fs::symlink_metadata(&path).is_ok_and(|m| (m.dev(), m.ino()) == self.identity) { let _ = std::fs::remove_file(path); }
    }
}
impl PartialUpload {
    fn create(dir: Directory) -> Result<Self, ApiError> {
        use std::io::Read;
        let mut random = [0u8; 16];
        File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut random)).map_err(io_error)?;
        let part = format!(".upload-{}.part", random.iter().map(|b| format!("{b:02x}")).collect::<String>());
        let file = OpenOptions::new().write(true).create_new(true).mode(0o666).open(dir.at(&part)).map_err(io_error)?;
        let meta = file.metadata().map_err(io_error)?;
        Ok(Self { dir, part, identity: (meta.dev(), meta.ino()), file: Some(file) })
    }
    fn publish(&self, file: &File, name: &str) -> Result<SavedFile, ApiError> {
        self.dir.saved(link_file(&self.dir, file, name).map_err(publish_error)?)
    }
}

async fn upload(path: PathBuf, create: bool, name: String, body: Body) -> Result<SavedFile, ApiError> {
    entry_name(&name)?;
    let mut partial = blocking(move || {
        if create { std::fs::create_dir_all(&path).map_err(io_error)?; }
        PartialUpload::create(Directory::open(&path)?)
    }).await?;
    let mut file = tokio::fs::File::from_std(partial.file.take().unwrap());
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        file.write_all(&chunk.map_err(|e| file_error("upload_failed", e.to_string()))?).await.map_err(io_error)?;
    }
    file.flush().await.map_err(io_error)?;
    let file = file.into_std().await;
    blocking(move || partial.publish(&file, &name)).await
}

fn kind(meta: &std::fs::Metadata) -> EntryKind {
    if meta.is_dir() { EntryKind::Directory } else if meta.is_file() { EntryKind::File } else if meta.file_type().is_symlink() { EntryKind::Symlink } else { EntryKind::Other }
}

impl App {
    fn file_path(&self, path: &str) -> Result<PathBuf, ApiError> {
        match path {
            "@transfer" => Ok(self.files_dir.clone()),
            "@home" => Ok(std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))),
            p if p.starts_with('/') && !p.contains('\0') => Ok(PathBuf::from(p)),
            _ => Err(file_error("invalid_path", "expected an absolute path, @home, or @transfer")),
        }
    }

    pub async fn browse_files(&self, query: FileQuery) -> Result<FileListing, ApiError> {
        let requested = query.path.as_deref().ok_or_else(|| file_error("invalid_path", "path is required"))?;
        let path = self.file_path(requested)?;
        let create = requested == "@transfer";
        blocking(move || {
            if create { std::fs::create_dir_all(&path).map_err(io_error)?; }
            let dir = Directory::open(&path)?;
            let mut entries = Vec::new();
            let mut omitted = 0;
            for entry in std::fs::read_dir(dir.at("")).map_err(io_error)? {
                let entry = entry.map_err(io_error)?;
                let Ok(name) = entry.file_name().into_string() else { omitted += 1; continue; };
                if name.starts_with(".upload-") && name.ends_with(".part") || !query.hidden && name.starts_with('.') { continue; }
                let meta = match std::fs::symlink_metadata(dir.at(&name)) {
                    Ok(m) => m,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(io_error(e)),
                };
                let k = kind(&meta);
                if matches!(k, EntryKind::Other) { continue; }
                let target_kind = if matches!(k, EntryKind::Symlink) { std::fs::metadata(dir.at(&name)).ok().map(|m| kind(&m)) } else { None };
                let modified_ms = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|t| t.as_millis() as u64).unwrap_or(0);
                entries.push(FileEntry { name, kind: k, target_kind, size: meta.len(), modified_ms });
            }
            let folder = |e: &FileEntry| matches!(e.kind, EntryKind::Directory) || matches!(e.target_kind, Some(EntryKind::Directory));
            entries.sort_by(|a, b| {
                let order = match query.sort { FileSort::Name => a.name.cmp(&b.name), FileSort::Size => a.size.cmp(&b.size), FileSort::Modified => a.modified_ms.cmp(&b.modified_ms) }.then_with(|| a.name.cmp(&b.name));
                folder(b).cmp(&folder(a)).then(if query.desc { order.reverse() } else { order })
            });
            let total = entries.len();
            let limit = query.limit.unwrap_or(100).clamp(1, 500);
            Ok(FileListing { path: dir.path()?, entries: entries.into_iter().skip(query.offset).take(limit).collect(), total, offset: query.offset, limit, omitted })
        }).await
    }

    pub async fn upload_file(&self, path: Option<&str>, name: &str, body: Body) -> Result<SavedFile, ApiError> {
        if path.is_none() { safe(name)?; }
        let requested = path.unwrap_or("@transfer");
        upload(self.file_path(requested)?, requested == "@transfer", name.to_owned(), body).await
    }

    pub async fn download_file(&self, path: Option<&str>, name: &str) -> Result<(Option<u64>, Body), ApiError> {
        if path.is_none() { return self.open_file(name).await; }
        let dir = self.file_path(path.unwrap())?;
        let name = entry_name(name)?.to_owned();
        blocking(move || { let dir = Directory::open(&dir)?; regular_file(&dir.at(&name), true) }).await.and_then(file_body)
    }

    pub async fn remove_file(&self, path: Option<&str>, name: &str) -> Result<(), ApiError> {
        if path.is_none() { safe(name)?; }
        let dir = self.file_path(path.unwrap_or("@transfer"))?;
        let name = entry_name(name)?.to_owned();
        blocking(move || { let dir = Directory::open(&dir)?; std::fs::remove_file(dir.at(&name)).map_err(io_error) }).await
    }

    pub async fn manage_file(&self, action: FileAction) -> Result<SavedFile, ApiError> {
        let (path, name) = match &action { FileAction::Mkdir { path, name } | FileAction::Rename { path, name, .. } => (self.file_path(path)?, entry_name(name)?.to_owned()) };
        if let FileAction::Rename { new_name, .. } = &action { entry_name(new_name)?; }
        blocking(move || {
            let dir = Directory::open(&path)?;
            match action {
                FileAction::Mkdir { .. } => { std::fs::create_dir(dir.at(&name)).map_err(io_error)?; dir.saved(name) }
                FileAction::Rename { new_name, .. } => {
                    let from = std::ffi::CString::new(name).unwrap();
                    let to = std::ffi::CString::new(new_name.as_str()).unwrap();
                    if unsafe { libc::renameat2(dir.0.as_raw_fd(), from.as_ptr(), dir.0.as_raw_fd(), to.as_ptr(), libc::RENAME_NOREPLACE) } != 0 {
                        let error = std::io::Error::last_os_error();
                        return Err(if matches!(error.raw_os_error(), Some(libc::EINVAL | libc::ENOSYS | libc::EOPNOTSUPP)) {
                            file_error("unsupported_operation", "this filesystem cannot rename without replacing an existing entry")
                        } else { io_error(error) });
                    }
                    dir.saved(new_name)
                }
            }
        }).await
    }
}

/// `XDG_DOWNLOAD_DIR` from `user-dirs.dirs`, else `~/Downloads`.
pub fn default_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    let config = std::env::var("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|_| Path::new(&home).join(".config"));
    std::fs::read_to_string(config.join("user-dirs.dirs"))
        .ok()
        .and_then(|s| s.lines().find_map(|l| l.strip_prefix("XDG_DOWNLOAD_DIR=")).map(|v| v.trim().trim_matches('"').replace("$HOME", &home)))
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(&home).join("Downloads"))
}

/// `$XDG_CACHE_HOME/browser-wayland/drops`, else `~/.cache/browser-wayland/drops`.
pub fn drops_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    std::env::var("XDG_CACHE_HOME").map(PathBuf::from).unwrap_or_else(|_| Path::new(&home).join(".cache")).join("browser-wayland/drops")
}

/// A name is one visible entry of the folder, nothing else (hidden names include our `.part` files).
fn safe(name: &str) -> Result<&str, ApiError> {
    if name.is_empty() || name.starts_with('.') || name.contains(['/', '\0']) { Err(ApiError::NoSuchFile) } else { Ok(name) }
}

/// `name`, then `stem (2).ext`, `stem (3).ext`, …
fn candidates(name: &str) -> impl Iterator<Item = String> + '_ {
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s, format!(".{e}")),
        _ => (name, String::new()),
    };
    std::iter::once(name.to_string()).chain((2..).map(move |n| format!("{stem} ({n}){ext}")))
}

impl App {
    /// The files in the folder (not its subfolders, hidden entries or symlinks), newest first.
    pub async fn files(&self) -> Result<Vec<FileInfo>, ApiError> {
        let dir = self.files_dir.clone();
        tokio::task::spawn_blocking(move || {
            let mut list: Vec<FileInfo> = match std::fs::read_dir(&dir) {
                Ok(entries) => entries
                    .flatten()
                    .filter_map(|e| {
                        let name = e.file_name().into_string().ok()?;
                        let meta = e.metadata().ok()?;
                        let modified_ms = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_millis() as u64;
                        (e.file_type().ok()?.is_file() && !name.starts_with('.')).then(|| FileInfo { name, size: meta.len(), modified_ms }) // no symlinks: they could point anywhere
                    })
                    .collect(),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => vec![],
                Err(e) => return Err(ApiError::Internal(e.to_string())),
            };
            list.sort_by(|a, b| b.modified_ms.cmp(&a.modified_ms).then_with(|| a.name.cmp(&b.name))); // newest first: what just landed
            Ok(list)
        })
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
    }

    /// Write an upload into the batch `batch`, a drag's or a paste's, for the application that will take it.
    pub async fn stage_file(&self, batch: &str, name: &str, body: Body) -> Result<String, ApiError> {
        self.store_into(&self.batch_dir(batch)?, name, body).await
    }

    fn batch_dir(&self, batch: &str) -> Result<PathBuf, ApiError> {
        Ok(self.drops_dir.join(safe(batch)?))
    }

    async fn store_into(&self, dir: &Path, name: &str, body: Body) -> Result<String, ApiError> {
        safe(name)?;
        Ok(upload(dir.to_owned(), true, name.to_owned(), body).await?.name)
    }

    /// A file from the folder as a streaming body with its size.
    pub async fn open_file(&self, name: &str) -> Result<(Option<u64>, Body), ApiError> {
        stream_file(&self.files_dir.join(safe(name)?)).await
    }

    /// Files of the transfer folder, or with `batch` of the batch a paste staged, as the desktop clipboard's
    /// URI list (a file manager's copy).
    pub fn set_clipboard_files(&self, names: &[String], batch: Option<&str>) -> Result<(), ApiError> {
        let dir = match batch {
            Some(b) => self.batch_dir(b)?,
            None => self.files_dir.clone(),
        };
        self.set_clipboard(crate::api::URI_LIST, uri_list(&dir, names)?.into())
    }

    /// The browser's drag as a compositor command. A drop names the files of its batch and drops their
    /// URIs; the batch comes back with the desktop's word (`DragEnded`). A name that isn't a plain file
    /// name cancels instead, so the desktop lets go; a cancel that names a batch (a drag whose upload half
    /// failed) sends its files to the transfer folder.
    pub(crate) fn drag_command(&self, msg: crate::protocol::DragMsg, viewers: &crate::Viewers) -> bw_core::Command {
        use crate::protocol::DragMsg;
        bw_core::Command::Drag(match msg {
            DragMsg::Start => bw_core::Drag::Start,
            DragMsg::Drop { batch, names } => match self.batch_dir(&batch).and_then(|dir| uri_list(&dir, &names)) {
                Ok(list) => bw_core::Drag::Drop { list, batch },
                Err(_) => bw_core::Drag::Cancel,
            },
            DragMsg::Cancel { batch } => {
                if let Some(b) = batch {
                    self.rescue_to(&b, viewers.sessions.values().filter(|s| s.key == crate::Key::Control).map(|s| s.events.clone()).collect());
                }
                bw_core::Drag::Cancel
            }
        })
    }

    /// Report rescue results to control-token sessions; the originating client recognizes its batch ID.
    pub fn rescue(&self, batch: &str) -> tokio::task::JoinHandle<bool> {
        let events = self.viewers.lock().unwrap().sessions.values().filter(|s| s.key == crate::Key::Control).map(|s| s.events.clone()).collect();
        self.rescue_to(batch, events)
    }

    fn rescue_to(&self, batch: &str, events: Vec<tokio::sync::mpsc::Sender<axum::body::Bytes>>) -> tokio::task::JoinHandle<bool> {
        let (dir, files_dir) = (self.batch_dir(batch), self.files_dir.clone());
        let batch = batch.to_owned();
        tokio::task::spawn_blocking(move || {
            let result = dir.and_then(|dir| rescue_files(&dir, &files_dir));
            let (saved, failed, error) = match result {
                Ok((saved, failed)) => (saved, failed, None),
                Err(e) => { tracing::warn!(error = %e, "file rescue failed"); (vec![], 1, Some(e.to_string())) },
            };
            let complete = failed == 0;
            let mut packet = vec![crate::protocol::FILE_RESULT];
            packet.extend(serde_json::to_vec(&serde_json::json!({ "batch": batch, "saved": saved, "failed": failed, "error": error })).unwrap());
            let packet: axum::body::Bytes = packet.into();
            for events in events { let _ = events.try_send(packet.clone()); }
            complete
        })
    }

    /// The `index`th `file://` URI of the list on the desktop clipboard, streamed: its name, size and body.
    pub async fn clipboard_file(&self, index: usize) -> Result<(String, Option<u64>, Body), ApiError> {
        let (mime, data) = self.clipboard().ok_or(ApiError::NoSuchFile)?;
        if mime != crate::api::URI_LIST {
            return Err(ApiError::NoSuchFile);
        }
        let uri = String::from_utf8_lossy(&data).lines().map(str::trim_end).filter(|l| l.starts_with("file://")).nth(index).map(str::to_string).ok_or(ApiError::NoSuchFile)?;
        let path = PathBuf::from(unpercent(uri.trim_start_matches("file://")));
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file").to_string();
        let (len, body) = stream_file(&path).await?;
        Ok((name, len, body))
    }


}

/// Link validated regular files, copying through temporary files only across filesystems.
fn rescue_files(dir: &Path, files_dir: &Path) -> Result<(Vec<SavedFile>, usize), ApiError> {
    std::fs::create_dir_all(files_dir).map_err(io_error)?;
    let source_dir = Directory::open(dir)?;
    let mut saved = Vec::new();
    let mut failed = 0;
    for entry in std::fs::read_dir(source_dir.at("")).map_err(io_error)? {
        let entry = entry.map_err(io_error)?;
        let Ok(name) = entry.file_name().into_string() else { failed += 1; continue; };
        if name.starts_with('.') { continue; }
        let result = (|| -> Result<SavedFile, ApiError> {
            let mut source = regular_file(&source_dir.at(&name), false)?;
            let target = Directory::open(files_dir)?;
            match link_file(&target, &source, &name) {
                Ok(name) => return target.saved(name),
                Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {},
                Err(e) => return Err(publish_error(e)),
            }
            let mut partial = PartialUpload::create(target)?;
            std::io::copy(&mut source, partial.file.as_mut().unwrap()).map_err(io_error)?;
            let saved = partial.publish(partial.file.as_ref().unwrap(), &name)?;
            // A staged copy stays in its cache batch until the normal sweep. External replacements
            // of a source name must not be unlinked as a side effect of copying an opened inode.
            Ok(saved)
        })();
        match result { Ok(file) => saved.push(file), Err(e) => { tracing::warn!(error = %e, "staged file could not be rescued"); failed += 1; } }
    }
    Ok((saved, failed))
}

/// A `text/uri-list` naming files of `dir`.
fn uri_list(dir: &Path, names: &[String]) -> Result<Vec<u8>, ApiError> {
    let mut list = String::new();
    for name in names {
        list += &format!("file://{}\r\n", percent_path(&dir.join(safe(name)?)));
    }
    Ok(list.into_bytes())
}

/// Every hour, staged batches older than a day go. Another instance may be sweeping the same directory,
/// so a batch already gone is no error.
pub async fn sweep(app: std::sync::Arc<App>) {
    loop {
        let dir = app.drops_dir.clone();
        let _ = tokio::task::spawn_blocking(move || {
            let old = std::time::SystemTime::now() - std::time::Duration::from_secs(24 * 3600);
            for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                if entry.metadata().is_ok_and(|m| m.is_dir() && m.modified().is_ok_and(|t| t < old)) {
                    let _ = std::fs::remove_dir_all(entry.path());
                }
            }
        })
        .await;
        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
    }
}

/// A regular file (not through a symlink) as a streaming body with its size (the size when opened: the
/// body stops there even if the file grows). Empty and procfs/sysfs files stream to EOF.
async fn stream_file(path: &Path) -> Result<(Option<u64>, Body), ApiError> {
    let path = path.to_owned();
    blocking(move || regular_file(&path, false)).await.and_then(file_body)
}

/// `filename*=UTF-8''…` percent-encoding for a Content-Disposition header.
pub fn percent(name: &str) -> String {
    name.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

/// A path as the body of a `file://` URI: every byte outside the unreserved set and `/` escaped.
fn percent_path(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().iter().map(|&b| if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

/// The reverse of `percent_path` for a URI's path (bytes, so any file name survives).
fn unpercent(s: &str) -> std::ffi::OsString {
    use std::os::unix::ffi::OsStringExt;
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() && let Ok(v) = u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("zz"), 16) {
            out.push(v);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    std::ffi::OsString::from_vec(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_publication_and_cleanup() {
        let root = std::env::temp_dir().join(format!("bw-file-fds-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir(&root).unwrap();
        let mut partial = PartialUpload::create(Directory::open(&root).unwrap()).unwrap();
        use std::io::Write;
        partial.file.as_mut().unwrap().write_all(b"original").unwrap();
        let old = root.join("old-part");
        std::fs::rename(partial.dir.at(&partial.part), &old).unwrap();
        let replacement = root.join(&partial.part);
        std::fs::write(&replacement, b"replacement").unwrap();
        let saved = partial.publish(partial.file.as_ref().unwrap(), "saved").unwrap();
        assert_eq!(saved.directory, root.to_str().unwrap());
        assert_eq!(std::fs::read(saved.path).unwrap(), b"original");
        drop(partial);
        assert_eq!(std::fs::read(replacement).unwrap(), b"replacement");
        let file = regular_file(&old, false).unwrap();
        std::fs::remove_file(&old).unwrap();
        std::os::unix::fs::symlink("/dev/zero", &old).unwrap();
        assert!(regular_file(&old, true).is_err());
        use std::io::Read;
        let mut bytes = Vec::new();
        std::io::BufReader::new(file).read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"original");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rescue_links_or_copies_opened_files() {
        let root = std::env::temp_dir().join(format!("bw-rescue-{}", std::process::id()));
        let cross = PathBuf::from(format!("/dev/shm/bw-rescue-{}", std::process::id()));
        std::fs::create_dir_all(root.join("source")).unwrap();
        std::fs::create_dir_all(&cross).unwrap();
        let source = root.join("source/file");
        std::fs::write(&source, b"rescue").unwrap();
        let (saved, failed) = rescue_files(&root.join("source"), &root.join("target")).unwrap();
        assert_eq!(failed, 0);
        assert_eq!(std::fs::metadata(&source).unwrap().ino(), std::fs::metadata(&saved[0].path).unwrap().ino());
        let (saved, failed) = rescue_files(&root.join("source"), &cross).unwrap();
        assert_eq!(failed, 0);
        assert_eq!(std::fs::read(&saved[0].path).unwrap(), b"rescue");
        assert_ne!(std::fs::metadata(&source).unwrap().dev(), std::fs::metadata(&saved[0].path).unwrap().dev());
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(cross).unwrap();
    }

    #[test]
    fn names() {
        assert!(safe("a.png").is_ok());
        for bad in ["", ".", "..", ".hidden", "a/b", "a\0"] {
            assert!(safe(bad).is_err(), "{bad:?}");
        }
        assert_eq!(candidates("x.tar.gz").take(3).collect::<Vec<_>>(), ["x.tar.gz", "x.tar (2).gz", "x.tar (3).gz"]);
        assert_eq!(candidates("noext").nth(1).unwrap(), "noext (2)");
        assert_eq!(percent("a b/ü.png"), "a%20b%2F%C3%BC.png");
        assert_eq!(percent_path(Path::new("/home/x/a b.png")), "/home/x/a%20b.png");
        assert_eq!(unpercent("/home/x/a%20b%C3%BC.png"), std::ffi::OsString::from("/home/x/a bü.png"));
    }
}
