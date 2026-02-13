use anyhow::{Context, Result, bail};
use base64::Engine;
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
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

    /// Validate and resolve a download path. Returns the canonical path.
    /// Rejects paths outside the user's home directory, symlinks that escape home,
    /// files that don't exist, and files exceeding the size limit.
    pub fn validate_download_path(&self, path: &str) -> Result<PathBuf> {
        if path.is_empty() {
            bail!("Empty path");
        }

        if path.contains('\0') {
            bail!("Path contains null byte");
        }

        let requested = Path::new(path);

        // Resolve to absolute path relative to home dir if not already absolute
        let abs_path = if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            self.home_dir.join(requested)
        };

        // Canonicalize resolves symlinks and .. components
        let canonical = abs_path
            .canonicalize()
            .with_context(|| format!("File not found: {}", abs_path.display()))?;

        // Must be under the user's home directory (after symlink resolution)
        let canonical_home = self
            .home_dir
            .canonicalize()
            .context("Cannot resolve home directory")?;
        if !canonical.starts_with(&canonical_home) {
            bail!(
                "Access denied: {} is outside home directory",
                canonical.display()
            );
        }

        // Must be a regular file (not a directory, device, etc.)
        let metadata = fs::metadata(&canonical)
            .with_context(|| format!("Cannot stat: {}", canonical.display()))?;
        if !metadata.is_file() {
            bail!("Not a regular file: {}", canonical.display());
        }

        if metadata.len() > MAX_FILE_SIZE {
            bail!(
                "File too large: {} bytes (max {} bytes)",
                metadata.len(),
                MAX_FILE_SIZE
            );
        }

        Ok(canonical)
    }

    /// Handle a file download request. Reads the file, chunks it into 16KB pieces,
    /// base64-encodes each chunk, and sends JSON messages via the provided function.
    pub fn handle_download_request(&self, path: &str, send_fn: &dyn Fn(String)) -> Result<()> {
        let id = uuid::Uuid::new_v4().to_string();

        let canonical = match self.validate_download_path(path) {
            Ok(p) => p,
            Err(e) => {
                let error_msg = serde_json::json!({
                    "t": "fde",
                    "id": id,
                    "error": e.to_string(),
                });
                send_fn(error_msg.to_string());
                return Err(e);
            }
        };

        let filename = canonical
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("download");

        let metadata = fs::metadata(&canonical)?;
        let file_size = metadata.len();

        // Send download start
        let start_msg = serde_json::json!({
            "t": "fds",
            "id": id,
            "name": filename,
            "size": file_size,
        });
        send_fn(start_msg.to_string());

        // Read and send chunks
        const DOWNLOAD_CHUNK_SIZE: usize = 16 * 1024;
        let mut file = fs::File::open(&canonical)
            .with_context(|| format!("Failed to open: {}", canonical.display()))?;
        let mut buf = vec![0u8; DOWNLOAD_CHUNK_SIZE];

        loop {
            let n = file.read(&mut buf).context("Failed to read file")?;
            if n == 0 {
                break;
            }
            let b64 = base64::engine::general_purpose::STANDARD.encode(&buf[..n]);
            let chunk_msg = serde_json::json!({
                "t": "fdc",
                "id": id,
                "data": b64,
            });
            send_fn(chunk_msg.to_string());
        }

        // Send download done
        let done_msg = serde_json::json!({
            "t": "fdd",
            "id": id,
        });
        send_fn(done_msg.to_string());

        info!(
            id,
            path = %canonical.display(),
            size = file_size,
            "File download sent to browser"
        );

        Ok(())
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

    #[test]
    fn download_validate_rejects_outside_home() {
        let dir = std::env::temp_dir().join(format!("beam-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();

        let mgr = FileTransferManager::new(dir.clone());
        let result = mgr.validate_download_path("/etc/passwd");
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("outside home directory")
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn download_validate_rejects_symlink_escape() {
        let dir = std::env::temp_dir().join(format!("beam-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();

        // Create a symlink that points outside home
        let symlink_path = dir.join("escape");
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/passwd", &symlink_path).unwrap();

        let mgr = FileTransferManager::new(dir.clone());
        let result = mgr.validate_download_path(symlink_path.to_str().unwrap());
        assert!(result.is_err());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn download_validate_rejects_nonexistent() {
        let dir = std::env::temp_dir().join(format!("beam-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();

        let mgr = FileTransferManager::new(dir.clone());
        let result = mgr.validate_download_path(&dir.join("nonexistent.txt").to_string_lossy());
        assert!(result.is_err());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn download_validate_rejects_directory() {
        let dir = std::env::temp_dir().join(format!("beam-test-{}", uuid::Uuid::new_v4()));
        let subdir = dir.join("subdir");
        fs::create_dir_all(&subdir).unwrap();

        let mgr = FileTransferManager::new(dir.clone());
        let result = mgr.validate_download_path(subdir.to_str().unwrap());
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Not a regular file")
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn download_validate_accepts_valid_file() {
        let dir = std::env::temp_dir().join(format!("beam-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();

        let test_file = dir.join("test.txt");
        fs::write(&test_file, b"hello").unwrap();

        let mgr = FileTransferManager::new(dir.clone());
        let result = mgr.validate_download_path(test_file.to_str().unwrap());
        assert!(result.is_ok());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn download_validate_relative_path() {
        let dir = std::env::temp_dir().join(format!("beam-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();

        let test_file = dir.join("doc.pdf");
        fs::write(&test_file, b"pdf content").unwrap();

        let mgr = FileTransferManager::new(dir.clone());
        // Relative path should be resolved under home_dir
        let result = mgr.validate_download_path("doc.pdf");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), test_file.canonicalize().unwrap());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn download_roundtrip() {
        let dir = std::env::temp_dir().join(format!("beam-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();

        let content = b"Download test content!";
        let test_file = dir.join("download_me.txt");
        fs::write(&test_file, content).unwrap();

        let mgr = FileTransferManager::new(dir.clone());
        let messages = std::sync::Mutex::new(Vec::new());
        let send_fn = |msg: String| {
            messages.lock().unwrap().push(msg);
        };

        mgr.handle_download_request(test_file.to_str().unwrap(), &send_fn)
            .unwrap();

        let msgs = messages.lock().unwrap();
        assert!(msgs.len() >= 3); // start + at least 1 chunk + done

        // Verify start message
        let start: serde_json::Value = serde_json::from_str(&msgs[0]).unwrap();
        assert_eq!(start["t"], "fds");
        assert_eq!(start["name"], "download_me.txt");
        assert_eq!(start["size"], content.len() as u64);

        // Verify chunk(s) — decode and reconstruct
        let mut reconstructed = Vec::new();
        for msg in &msgs[1..msgs.len() - 1] {
            let chunk: serde_json::Value = serde_json::from_str(msg).unwrap();
            assert_eq!(chunk["t"], "fdc");
            let b64 = chunk["data"].as_str().unwrap();
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(b64)
                .unwrap();
            reconstructed.extend_from_slice(&decoded);
        }
        assert_eq!(reconstructed, content);

        // Verify done message
        let done: serde_json::Value = serde_json::from_str(msgs.last().unwrap()).unwrap();
        assert_eq!(done["t"], "fdd");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn download_error_sends_fde_message() {
        let dir = std::env::temp_dir().join(format!("beam-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();

        let mgr = FileTransferManager::new(dir.clone());
        let messages = std::sync::Mutex::new(Vec::new());
        let send_fn = |msg: String| {
            messages.lock().unwrap().push(msg);
        };

        // Request a non-existent file — should send fde message
        let _ = mgr.handle_download_request("/etc/passwd", &send_fn);

        let msgs = messages.lock().unwrap();
        assert_eq!(msgs.len(), 1);
        let err: serde_json::Value = serde_json::from_str(&msgs[0]).unwrap();
        assert_eq!(err["t"], "fde");
        assert!(
            err["error"]
                .as_str()
                .unwrap()
                .contains("outside home directory")
        );

        fs::remove_dir_all(&dir).ok();
    }
}
