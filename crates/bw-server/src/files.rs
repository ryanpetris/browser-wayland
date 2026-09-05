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

/// A name is one entry of the folder, nothing else.
fn safe(name: &str) -> Result<&str, ApiError> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) { Err(ApiError::NotFound) } else { Ok(name) }
}

/// `name`, or `stem (n).ext` for the first n that isn't taken.
fn unique(dir: &Path, name: &str) -> String {
    if !dir.join(name).exists() {
        return name.to_string();
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s, format!(".{e}")),
        _ => (name, String::new()),
    };
    (2..).map(|n| format!("{stem} ({n}){ext}")).find(|c| !dir.join(c).exists()).unwrap()
}

impl App {
    /// The files in the folder (not its subfolders or hidden entries), by name.
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
                        (meta.is_file() && !name.starts_with('.')).then(|| FileInfo { name, size: meta.len(), modified_ms })
                    })
                    .collect(),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => vec![],
                Err(e) => return Err(ApiError::Internal(e.to_string())),
            };
            list.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(list)
        })
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
    }

    /// Write an upload into the folder under `name` (made unique), through a `.part` file so a half
    /// upload never looks complete. Returns the final name.
    pub async fn store_file(&self, name: &str, body: Body) -> Result<String, ApiError> {
        let name = safe(name)?;
        tokio::fs::create_dir_all(&self.files_dir).await.map_err(|e| ApiError::Internal(e.to_string()))?;
        let final_name = unique(&self.files_dir, name);
        let (tmp, path) = (self.files_dir.join(format!(".{final_name}.part")), self.files_dir.join(&final_name));
        let written = async {
            let mut file = tokio::fs::File::create(&tmp).await?;
            let mut stream = body.into_data_stream();
            while let Some(chunk) = stream.next().await {
                file.write_all(&chunk.map_err(std::io::Error::other)?).await?;
            }
            file.flush().await?;
            tokio::fs::rename(&tmp, &path).await
        }
        .await;
        if let Err(e) = written {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(ApiError::Internal(e.to_string()));
        }
        Ok(final_name)
    }

    /// A file from the folder as a streaming body with its size.
    pub async fn open_file(&self, name: &str) -> Result<(u64, Body), ApiError> {
        let path = self.files_dir.join(safe(name)?);
        let file = tokio::fs::File::open(&path).await.map_err(|_| ApiError::NotFound)?;
        let len = file.metadata().await.map_err(|e| ApiError::Internal(e.to_string()))?.len();
        Ok((len, Body::from_stream(tokio_util::io::ReaderStream::new(file))))
    }

    pub async fn delete_file(&self, name: &str) -> Result<(), ApiError> {
        tokio::fs::remove_file(self.files_dir.join(safe(name)?)).await.map_err(|_| ApiError::NotFound)
    }
}

/// `filename*=UTF-8''…` percent-encoding for a Content-Disposition header.
pub fn percent(name: &str) -> String {
    name.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert!(safe("a.png").is_ok());
        for bad in ["", ".", "..", "a/b", "a\0"] {
            assert!(safe(bad).is_err(), "{bad:?}");
        }
        let dir = std::env::temp_dir().join(format!("bw-files-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(unique(&dir, "x.tar.gz"), "x.tar.gz");
        std::fs::write(dir.join("x.tar.gz"), b"").unwrap();
        assert_eq!(unique(&dir, "x.tar.gz"), "x.tar (2).gz");
        std::fs::write(dir.join("noext"), b"").unwrap();
        assert_eq!(unique(&dir, "noext"), "noext (2)");
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(percent("a b/ü.png"), "a%20b%2F%C3%BC.png");
    }
}
