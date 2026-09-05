//! File transfer: a folder (the XDG download directory unless `--files-dir` says otherwise) that files
//! dropped on the page land in and that the page lists and downloads from. Names are plain file names in
//! that folder; a name in use gets " (2)" before its extension.

use std::{
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use axum::body::Body;
use futures_util::StreamExt;
use schemars::JsonSchema;
use serde::Serialize;
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

static UPLOADS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

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

    /// Write an upload into the folder under `name`, or the first `name (n)` that is free, through a
    /// `.part` file of its own so a half upload never looks complete and two uploads never meet: the
    /// final name is claimed with a hard link, which fails if it exists. Returns the name it got.
    pub async fn store_file(&self, name: &str, body: Body) -> Result<String, ApiError> {
        let name = safe(name)?;
        tokio::fs::create_dir_all(&self.files_dir).await.map_err(|e| ApiError::Internal(e.to_string()))?;
        let tmp = self.files_dir.join(format!(".upload-{}-{}.part", std::process::id(), UPLOADS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
        let written = async {
            let mut file = tokio::fs::OpenOptions::new().write(true).create_new(true).open(&tmp).await?;
            let mut stream = body.into_data_stream();
            while let Some(chunk) = stream.next().await {
                file.write_all(&chunk.map_err(std::io::Error::other)?).await?;
            }
            file.flush().await?;
            for candidate in candidates(name) {
                match tokio::fs::hard_link(&tmp, self.files_dir.join(&candidate)).await {
                    Ok(()) => return Ok(candidate),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(e) => return Err(e),
                }
            }
            unreachable!("candidates never end")
        }
        .await;
        let _ = tokio::fs::remove_file(&tmp).await;
        written.map_err(|e| ApiError::Internal(e.to_string()))
    }

    /// A file from the folder as a streaming body with its size.
    pub async fn open_file(&self, name: &str) -> Result<(u64, Body), ApiError> {
        stream_file(&self.files_dir.join(safe(name)?)).await
    }

    /// Files of the folder as the desktop clipboard's URI list (a file manager's copy).
    pub fn set_clipboard_files(&self, names: &[&str]) -> Result<(), ApiError> {
        let mut list = String::new();
        for name in names {
            let path = self.files_dir.join(safe(name)?);
            list += &format!("file://{}\r\n", percent_path(&path));
        }
        self.set_clipboard(crate::api::URI_LIST, list.into())
    }

    /// The `index`th `file://` URI of the list on the desktop clipboard, streamed: its name, size and body.
    pub async fn clipboard_file(&self, index: usize) -> Result<(String, u64, Body), ApiError> {
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

    pub async fn delete_file(&self, name: &str) -> Result<(), ApiError> {
        tokio::fs::remove_file(self.files_dir.join(safe(name)?)).await.map_err(|_| ApiError::NoSuchFile)
    }
}

/// A regular file (not through a symlink) as a streaming body with its size (the size when opened: the
/// body stops there even if the file grows).
async fn stream_file(path: &Path) -> Result<(u64, Body), ApiError> {
    let regular = tokio::fs::symlink_metadata(path).await.is_ok_and(|m| m.is_file());
    let file = if regular { tokio::fs::File::open(path).await.map_err(|_| ApiError::NoSuchFile)? } else { return Err(ApiError::NoSuchFile) };
    let len = file.metadata().await.map_err(|e| ApiError::Internal(e.to_string()))?.len();
    Ok((len, Body::from_stream(tokio_util::io::ReaderStream::new(tokio::io::AsyncReadExt::take(file, len)))))
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
