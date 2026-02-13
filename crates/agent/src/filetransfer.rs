use anyhow::{Context, Result, bail};
use base64::Engine;
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

const MAX_FILE_SIZE: u64 = 100 * 1024 * 1024; // 100 MB
const MAX_FILENAME_LEN: usize = 255;
const MAX_CONCURRENT_TRANSFERS: usize = 8;

struct ActiveTransfer {
    name: String,
    size: u64,
    received: u64,
    temp_path: PathBuf,
    file: fs::File,
}

pub struct FileTransferManager {
    transfers: HashMap<String, ActiveTransfer>,
    home_dir: PathBuf,
}

impl FileTransferManager {
    pub fn new(home_dir: PathBuf) -> Self {
        Self {
            transfers: HashMap::new(),
            home_dir,
        }
    }

    pub fn handle_file_start(&mut self, id: &str, name: &str, size: u64) -> Result<()> {
        if self.transfers.len() >= MAX_CONCURRENT_TRANSFERS {
            bail!("Too many concurrent transfers");
        }

        if self.transfers.contains_key(id) {
            bail!("Transfer {id} already in progress");
        }

        let sanitized = sanitize_filename(name)?;

        if size > MAX_FILE_SIZE {
            bail!(
                "File too large: {} bytes (max {} bytes)",
                size,
                MAX_FILE_SIZE
            );
        }

        let temp_path = PathBuf::from(format!("/tmp/beam-transfer-{id}"));
        let file = fs::File::create(&temp_path)
            .with_context(|| format!("Failed to create temp file: {}", temp_path.display()))?;

        info!(id, name = sanitized, size, "File transfer started");

        self.transfers.insert(
            id.to_string(),
            ActiveTransfer {
                name: sanitized,
                size,
                received: 0,
                temp_path,
                file,
            },
        );

        Ok(())
    }

    pub fn handle_file_chunk(&mut self, id: &str, data: &str) -> Result<()> {
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(data)
            .context("Invalid base64 data")?;

        let transfer = self
            .transfers
            .get_mut(id)
            .with_context(|| format!("No active transfer: {id}"))?;

        transfer.received += decoded.len() as u64;
        if transfer.received > transfer.size {
            let received = transfer.received;
            let size = transfer.size;
            let t = self.transfers.remove(id).unwrap();
            let _ = fs::remove_file(&t.temp_path);
            bail!("Received more data than declared size ({received} > {size})");
        }

        transfer
            .file
            .write_all(&decoded)
            .context("Failed to write chunk to temp file")?;

        Ok(())
    }

    pub fn handle_file_done(&mut self, id: &str) -> Result<()> {
        let transfer = self
            .transfers
            .remove(id)
            .with_context(|| format!("No active transfer: {id}"))?;

        drop(transfer.file); // Close the file handle

        let downloads_dir = self.home_dir.join("Downloads");
        fs::create_dir_all(&downloads_dir).context("Failed to create ~/Downloads")?;

        let dest = unique_path(&downloads_dir, &transfer.name);

        fs::rename(&transfer.temp_path, &dest)
            .or_else(|_| {
                // rename fails across filesystems; fall back to copy + remove
                fs::copy(&transfer.temp_path, &dest)?;
                fs::remove_file(&transfer.temp_path)?;
                Ok::<(), std::io::Error>(())
            })
            .with_context(|| format!("Failed to move file to {}", dest.display()))?;

        info!(
            id,
            name = transfer.name,
            size = transfer.received,
            dest = %dest.display(),
            "File transfer complete"
        );

        Ok(())
    }

    pub fn cleanup(&mut self) {
        for (id, transfer) in self.transfers.drain() {
            warn!(id, name = transfer.name, "Cleaning up incomplete transfer");
            let _ = fs::remove_file(&transfer.temp_path);
        }
    }
}

impl Drop for FileTransferManager {
    fn drop(&mut self) {
        self.cleanup();
    }
}

/// Sanitize a filename: reject path traversal, null bytes, and excessive length.
/// Returns the sanitized basename (no directory components).
fn sanitize_filename(name: &str) -> Result<String> {
    if name.is_empty() {
        bail!("Empty filename");
    }

    if name.contains('\0') {
        bail!("Filename contains null byte");
    }

    // Extract just the filename component (strip any directory path)
    let basename = Path::new(name)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(name);

    if basename.is_empty() || basename == "." || basename == ".." {
        bail!("Invalid filename: {name}");
    }

    if basename.contains('/') || basename.contains('\\') || basename.contains('\0') {
        bail!("Filename contains path separators or null bytes");
    }

    if basename.len() > MAX_FILENAME_LEN {
        bail!(
            "Filename too long: {} chars (max {})",
            basename.len(),
            MAX_FILENAME_LEN
        );
    }

    // Reject hidden files starting with .
    if basename.starts_with('.') {
        bail!("Hidden filenames not allowed");
    }

    Ok(basename.to_string())
}

/// Generate a unique file path by appending (1), (2), etc. if the file exists.
fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }

    let stem = Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(name);
    let ext = Path::new(name).extension().and_then(|e| e.to_str());

    for i in 1..=999 {
        let new_name = match ext {
            Some(e) => format!("{stem}({i}).{e}"),
            None => format!("{stem}({i})"),
        };
        let candidate = dir.join(&new_name);
        if !candidate.exists() {
            return candidate;
        }
    }

    // Fallback: use a UUID suffix
    let uuid = uuid::Uuid::new_v4();
    let new_name = match ext {
        Some(e) => format!("{stem}-{uuid}.{e}"),
        None => format!("{stem}-{uuid}"),
    };
    dir.join(new_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_valid_filename() {
        assert_eq!(sanitize_filename("hello.txt").unwrap(), "hello.txt");
        assert_eq!(
            sanitize_filename("my file (1).pdf").unwrap(),
            "my file (1).pdf"
        );
    }

    #[test]
    fn sanitize_strips_directory() {
        assert_eq!(sanitize_filename("some/path/file.txt").unwrap(), "file.txt");
        assert_eq!(
            sanitize_filename("/absolute/path/doc.pdf").unwrap(),
            "doc.pdf"
        );
    }

    #[test]
    fn sanitize_rejects_path_traversal() {
        assert!(sanitize_filename("..").is_err());
        assert!(
            sanitize_filename("../../../etc/passwd").unwrap_or_default() != "../../../etc/passwd"
        );
        // After stripping to basename, ../../../etc/passwd -> passwd (valid)
        assert_eq!(sanitize_filename("../../../etc/passwd").unwrap(), "passwd");
    }

    #[test]
    fn sanitize_rejects_null_bytes() {
        assert!(sanitize_filename("file\0.txt").is_err());
    }

    #[test]
    fn sanitize_rejects_empty() {
        assert!(sanitize_filename("").is_err());
    }

    #[test]
    fn sanitize_rejects_too_long() {
        let long_name = "a".repeat(256);
        assert!(sanitize_filename(&long_name).is_err());
    }

    #[test]
    fn sanitize_rejects_hidden_files() {
        assert!(sanitize_filename(".bashrc").is_err());
        assert!(sanitize_filename(".env").is_err());
        // .ssh/authorized_keys strips to "authorized_keys" which is valid
        assert_eq!(
            sanitize_filename(".ssh/authorized_keys").unwrap(),
            "authorized_keys"
        );
    }

    #[test]
    fn sanitize_rejects_dot_dot() {
        assert!(sanitize_filename("..").is_err());
        assert!(sanitize_filename(".").is_err());
    }

    #[test]
    fn unique_path_no_conflict() {
        let dir = std::env::temp_dir();
        let name = format!("beam-test-unique-{}.txt", uuid::Uuid::new_v4());
        let path = unique_path(&dir, &name);
        assert_eq!(path, dir.join(&name));
    }

    #[test]
    fn size_limit_validation() {
        let dir = std::env::temp_dir().join(format!("beam-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();

        let mut mgr = FileTransferManager::new(dir.clone());
        let result = mgr.handle_file_start("test1", "big.bin", MAX_FILE_SIZE + 1);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("too large"));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn full_transfer_roundtrip() {
        let dir = std::env::temp_dir().join(format!("beam-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();

        let mut mgr = FileTransferManager::new(dir.clone());

        let content = b"Hello, file transfer!";
        let b64 = base64::engine::general_purpose::STANDARD.encode(content);

        mgr.handle_file_start("t1", "test.txt", content.len() as u64)
            .unwrap();
        mgr.handle_file_chunk("t1", &b64).unwrap();
        mgr.handle_file_done("t1").unwrap();

        let dest = dir.join("Downloads").join("test.txt");
        assert!(dest.exists());
        assert_eq!(fs::read(&dest).unwrap(), content);

        fs::remove_dir_all(&dir).ok();
    }
}
