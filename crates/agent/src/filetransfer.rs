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

/// Trait abstracting the inbound-file-transfer surface that the input
/// callback drives.
///
/// Production uses [`FileTransferManager`] which writes to /tmp + ~/Downloads.
/// Tests can swap in a recording mock so the callback dispatch can be
/// driven without filesystem side effects.
pub trait FileSink: Send {
    fn handle_file_start(&mut self, id: &str, name: &str, size: u64) -> Result<()>;
    fn handle_file_chunk(&mut self, id: &str, data: &str) -> Result<()>;
    fn handle_file_done(&mut self, id: &str) -> Result<()>;
}

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

impl FileSink for FileTransferManager {
    fn handle_file_start(&mut self, id: &str, name: &str, size: u64) -> Result<()> {
        FileTransferManager::handle_file_start(self, id, name, size)
    }
    fn handle_file_chunk(&mut self, id: &str, data: &str) -> Result<()> {
        FileTransferManager::handle_file_chunk(self, id, data)
    }
    fn handle_file_done(&mut self, id: &str) -> Result<()> {
        FileTransferManager::handle_file_done(self, id)
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

    // --- handle_file_start error branches ---

    #[test]
    fn handle_file_start_rejects_duplicate_id() {
        // A second start with the same id while the first is still active must
        // fail rather than overwrite the in-flight transfer's File handle.
        let dir = std::env::temp_dir().join(format!("beam-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();

        let mut mgr = FileTransferManager::new(dir.clone());
        mgr.handle_file_start("dup-id", "a.txt", 10).unwrap();
        let result = mgr.handle_file_start("dup-id", "b.txt", 10);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("already in progress"),
            "Error must mention the in-progress collision",
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn handle_file_start_rejects_when_concurrent_limit_reached() {
        // MAX_CONCURRENT_TRANSFERS is 8; the 9th start should be rejected.
        // We stop at the gate before any disk write — temp files for the first
        // 8 will be cleaned up by the dir teardown.
        let dir = std::env::temp_dir().join(format!("beam-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();

        let mut mgr = FileTransferManager::new(dir.clone());
        // Fill the slot table to capacity. Use unique ids per loop iteration.
        for i in 0..MAX_CONCURRENT_TRANSFERS {
            let id = format!("concur-{i}");
            mgr.handle_file_start(&id, &format!("f{i}.bin"), 10)
                .unwrap();
        }
        let result = mgr.handle_file_start("one-too-many", "extra.bin", 10);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Too many concurrent")
        );

        // Cleanup: drop manager (Drop impl removes temp files).
        drop(mgr);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn handle_file_start_rejects_invalid_filename() {
        // Hidden filenames are rejected by sanitize_filename; the error must
        // surface from handle_file_start.
        let dir = std::env::temp_dir().join(format!("beam-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();

        let mut mgr = FileTransferManager::new(dir.clone());
        let result = mgr.handle_file_start("hidden-id", ".secret", 10);
        assert!(result.is_err());

        fs::remove_dir_all(&dir).ok();
    }

    // --- handle_file_chunk error branches ---

    #[test]
    fn handle_file_chunk_rejects_unknown_id() {
        let dir = std::env::temp_dir().join(format!("beam-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();

        let mut mgr = FileTransferManager::new(dir.clone());
        let b64 = base64::engine::general_purpose::STANDARD.encode(b"data");
        let result = mgr.handle_file_chunk("unknown", &b64);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("No active transfer")
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn handle_file_chunk_rejects_invalid_base64() {
        let dir = std::env::temp_dir().join(format!("beam-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();

        let mut mgr = FileTransferManager::new(dir.clone());
        mgr.handle_file_start("b64-id", "file.bin", 10).unwrap();
        // "!!!" is not valid base64 input
        let result = mgr.handle_file_chunk("b64-id", "!!!not-valid-base64!!!");
        assert!(result.is_err());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn handle_file_chunk_rejects_payload_exceeding_declared_size() {
        // The size limit is enforced cumulatively: declared 5 bytes but a
        // single chunk of 10 bytes overshoots → bail + remove temp file.
        let dir = std::env::temp_dir().join(format!("beam-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();

        let mut mgr = FileTransferManager::new(dir.clone());
        mgr.handle_file_start("over", "file.bin", 5).unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(b"too-many-bytes");
        let result = mgr.handle_file_chunk("over", &b64);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("more data than declared"),
            "Expected over-size error, got: {err}"
        );

        // The transfer must be evicted after the overshoot so subsequent chunks
        // also fail with "No active transfer".
        let b64_b = base64::engine::general_purpose::STANDARD.encode(b"x");
        let followup = mgr.handle_file_chunk("over", &b64_b);
        assert!(followup.is_err());
        assert!(
            followup
                .unwrap_err()
                .to_string()
                .contains("No active transfer"),
            "Transfer should have been evicted after overshoot"
        );

        fs::remove_dir_all(&dir).ok();
    }

    // --- handle_file_done error branch ---

    #[test]
    fn handle_file_done_rejects_unknown_id() {
        let dir = std::env::temp_dir().join(format!("beam-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();

        let mut mgr = FileTransferManager::new(dir.clone());
        let result = mgr.handle_file_done("unknown-done");
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("No active transfer")
        );

        fs::remove_dir_all(&dir).ok();
    }

    // --- cleanup / Drop ---

    #[test]
    fn drop_cleans_up_incomplete_transfers() {
        // Starting a transfer and then dropping the manager must remove the
        // temp file so /tmp doesn't accumulate orphans across crashes.
        let dir = std::env::temp_dir().join(format!("beam-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();

        let temp_id = format!("drop-test-{}", uuid::Uuid::new_v4());
        let temp_path = PathBuf::from(format!("/tmp/beam-transfer-{temp_id}"));
        // Pre-clean in case a previous run left a stale file
        let _ = fs::remove_file(&temp_path);

        {
            let mut mgr = FileTransferManager::new(dir.clone());
            mgr.handle_file_start(&temp_id, "f.bin", 10).unwrap();
            assert!(temp_path.exists(), "temp file should exist after start");
        } // Drop runs cleanup()

        assert!(
            !temp_path.exists(),
            "temp file must be removed by Drop::cleanup()"
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn explicit_cleanup_removes_all_pending() {
        let dir = std::env::temp_dir().join(format!("beam-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();

        let mut mgr = FileTransferManager::new(dir.clone());
        let ids: Vec<String> = (0..3)
            .map(|i| format!("explicit-cleanup-{}-{i}", uuid::Uuid::new_v4()))
            .collect();
        for id in &ids {
            mgr.handle_file_start(id, "f.bin", 10).unwrap();
        }
        let temp_paths: Vec<PathBuf> = ids
            .iter()
            .map(|id| PathBuf::from(format!("/tmp/beam-transfer-{id}")))
            .collect();
        for p in &temp_paths {
            assert!(p.exists());
        }

        mgr.cleanup();

        for p in &temp_paths {
            assert!(!p.exists(), "cleanup should remove every pending temp file");
        }
        // The transfers map should be empty so subsequent operations succeed
        // for the same id (no "already in progress" error).
        mgr.handle_file_start(&ids[0], "f.bin", 10).unwrap();
        // Cleanup remaining
        drop(mgr);
        fs::remove_dir_all(&dir).ok();
    }

    // --- validate_download_path: empty + null cases ---

    #[test]
    fn download_validate_rejects_empty_path() {
        let dir = std::env::temp_dir().join(format!("beam-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();

        let mgr = FileTransferManager::new(dir.clone());
        let result = mgr.validate_download_path("");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Empty path"));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn download_validate_rejects_path_with_null_byte() {
        let dir = std::env::temp_dir().join(format!("beam-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();

        let mgr = FileTransferManager::new(dir.clone());
        let result = mgr.validate_download_path("file\0name.txt");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("null byte"));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn download_validate_rejects_oversized_file() {
        // A file larger than MAX_FILE_SIZE must be rejected. Use a sparse file
        // to avoid actually writing 100MB.
        let dir = std::env::temp_dir().join(format!("beam-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();

        let big = dir.join("big.bin");
        let f = fs::File::create(&big).unwrap();
        f.set_len(MAX_FILE_SIZE + 1).unwrap();
        drop(f);

        let mgr = FileTransferManager::new(dir.clone());
        let result = mgr.validate_download_path(big.to_str().unwrap());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("too large"),
            "Expected size-limit error, got: {err}"
        );

        fs::remove_dir_all(&dir).ok();
    }

    // --- unique_path: collision branches ---

    #[test]
    fn unique_path_with_collision_appends_numeric_suffix() {
        // First file exists at name.txt → unique_path returns name(1).txt.
        let dir = std::env::temp_dir().join(format!("beam-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();

        // Seed existing
        let original = dir.join("doc.txt");
        fs::write(&original, b"existing").unwrap();

        let result = unique_path(&dir, "doc.txt");
        assert_eq!(result, dir.join("doc(1).txt"));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unique_path_with_extension_preserves_extension() {
        let dir = std::env::temp_dir().join(format!("beam-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();

        fs::write(dir.join("photo.jpg"), b"a").unwrap();
        fs::write(dir.join("photo(1).jpg"), b"b").unwrap();

        let result = unique_path(&dir, "photo.jpg");
        assert_eq!(result, dir.join("photo(2).jpg"));
        // Extension must be preserved on all suffix candidates.
        assert!(result.extension().is_some());
        assert_eq!(result.extension().unwrap(), "jpg");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unique_path_no_extension_skips_dot_in_suffix() {
        let dir = std::env::temp_dir().join(format!("beam-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();

        fs::write(dir.join("README"), b"a").unwrap();
        let result = unique_path(&dir, "README");
        // Extension branch in unique_path returns "{stem}({i})" with no dot.
        assert_eq!(result, dir.join("README(1)"));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unique_path_999_collisions_falls_back_to_uuid_suffix() {
        // Seed 999 colliding files. The 1000th attempt blows past the numeric
        // suffix range and must fall back to the UUID branch.
        let dir = std::env::temp_dir().join(format!("beam-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();

        fs::write(dir.join("x.bin"), b"a").unwrap();
        for i in 1..=999 {
            fs::write(dir.join(format!("x({i}).bin")), b"a").unwrap();
        }

        let result = unique_path(&dir, "x.bin");
        let result_name = result.file_name().unwrap().to_str().unwrap();
        // The fallback shape is "x-<uuid>.bin" — verify the prefix and suffix
        // and that the candidate slot doesn't already exist.
        assert!(
            result_name.starts_with("x-"),
            "Expected uuid-suffixed name, got: {result_name}"
        );
        assert!(
            result_name.ends_with(".bin"),
            "UUID fallback must keep the extension, got: {result_name}"
        );
        assert!(
            !result.exists(),
            "Returned path must not collide with anything on disk"
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unique_path_999_collisions_no_extension_falls_back_to_uuid_suffix() {
        // Same as above but for files with no extension — the UUID fallback
        // must take the "no ext" arm of the match.
        let dir = std::env::temp_dir().join(format!("beam-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();

        fs::write(dir.join("note"), b"a").unwrap();
        for i in 1..=999 {
            fs::write(dir.join(format!("note({i})")), b"a").unwrap();
        }

        let result = unique_path(&dir, "note");
        let result_name = result.file_name().unwrap().to_str().unwrap();
        assert!(
            result_name.starts_with("note-"),
            "Expected uuid-suffixed name, got: {result_name}"
        );
        // No extension means no trailing ".ext"
        assert!(
            !result_name.contains('.'),
            "UUID fallback for extensionless file must not invent an extension, got: {result_name}"
        );

        fs::remove_dir_all(&dir).ok();
    }

    // --- handle_file_done: rename to Downloads with collision ---

    #[test]
    fn handle_file_done_renames_with_unique_path_when_destination_exists() {
        // Seed Downloads/test.txt before completing a transfer to "test.txt".
        // unique_path appends (1) so the original file isn't overwritten.
        let dir = std::env::temp_dir().join(format!("beam-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let downloads = dir.join("Downloads");
        fs::create_dir_all(&downloads).unwrap();
        fs::write(downloads.join("test.txt"), b"PRE-EXISTING").unwrap();

        let mut mgr = FileTransferManager::new(dir.clone());
        let content = b"new content";
        let b64 = base64::engine::general_purpose::STANDARD.encode(content);
        mgr.handle_file_start("collide", "test.txt", content.len() as u64)
            .unwrap();
        mgr.handle_file_chunk("collide", &b64).unwrap();
        mgr.handle_file_done("collide").unwrap();

        // Pre-existing file untouched
        let pre = fs::read(downloads.join("test.txt")).unwrap();
        assert_eq!(pre, b"PRE-EXISTING");

        // New file landed at test(1).txt
        let new = fs::read(downloads.join("test(1).txt")).unwrap();
        assert_eq!(new, content);

        fs::remove_dir_all(&dir).ok();
    }

    // --- sanitize_filename: edge cases ---

    #[test]
    fn sanitize_rejects_pure_path_separator() {
        // A single "/" has no basename after strip → invalid filename.
        let result = sanitize_filename("/");
        assert!(result.is_err());
    }

    #[test]
    fn sanitize_rejects_filename_with_embedded_separator() {
        // Path::file_name strips the dirname, so "a/b.txt" → "b.txt"
        // is a valid pass-through. The literal "b\\.txt" (backslash, not a
        // separator on Unix) is what tests the embedded-slash-or-backslash
        // bail path — but since Path::file_name treats it as one component
        // on Unix, the bail clause for embedded separators rejects it.
        let result = sanitize_filename("evil\\name.txt");
        // On Unix, "\\" is a literal char in a single component; the bail
        // arm rejects it because basename.contains('\\').
        assert!(result.is_err());
    }

    #[test]
    fn sanitize_accepts_max_length_filename() {
        // Exactly MAX_FILENAME_LEN chars is accepted; one more is rejected.
        let at_limit = "a".repeat(MAX_FILENAME_LEN);
        assert!(sanitize_filename(&at_limit).is_ok());
        let over_limit = "a".repeat(MAX_FILENAME_LEN + 1);
        assert!(sanitize_filename(&over_limit).is_err());
    }

    #[test]
    fn handle_file_start_creates_temp_file_at_well_known_path() {
        // The temp file lands at /tmp/beam-transfer-<id> — the exact path is
        // load-bearing for the move-or-copy fallback in handle_file_done.
        let dir = std::env::temp_dir().join(format!("beam-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let unique_id = format!("path-test-{}", uuid::Uuid::new_v4());

        let mut mgr = FileTransferManager::new(dir.clone());
        mgr.handle_file_start(&unique_id, "f.bin", 10).unwrap();
        let expected = PathBuf::from(format!("/tmp/beam-transfer-{unique_id}"));
        assert!(
            expected.exists(),
            "Temp file should exist at well-known path"
        );

        drop(mgr); // cleanup
        fs::remove_dir_all(&dir).ok();
    }

    // --- FileSink trait impl exercise ---

    #[test]
    fn file_sink_trait_handle_file_start_delegates() {
        let dir = std::env::temp_dir().join(format!("beam-test-sink-start-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let mut mgr = FileTransferManager::new(dir.clone());
        let unique_id = format!("sink-start-{}", std::process::id());
        let sink: &mut dyn FileSink = &mut mgr;
        sink.handle_file_start(&unique_id, "x.bin", 5).unwrap();
        // Verify a temp file exists at the standard path.
        let expected = PathBuf::from(format!("/tmp/beam-transfer-{unique_id}"));
        assert!(expected.exists());
        // Drop will clean up.
        drop(mgr);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn file_sink_trait_handle_file_chunk_delegates() {
        let dir = std::env::temp_dir().join(format!("beam-test-sink-chunk-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let mut mgr = FileTransferManager::new(dir.clone());
        let unique_id = format!("sink-chunk-{}", std::process::id());
        let sink: &mut dyn FileSink = &mut mgr;
        sink.handle_file_start(&unique_id, "x.bin", 4).unwrap();
        // base64("AAA=") = [0, 0]
        sink.handle_file_chunk(&unique_id, "AAA=").unwrap();
        drop(mgr);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn file_sink_trait_handle_file_done_delegates() {
        let dir = std::env::temp_dir().join(format!("beam-test-sink-done-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let mut mgr = FileTransferManager::new(dir.clone());
        let unique_id = format!("sink-done-{}", std::process::id());
        let sink: &mut dyn FileSink = &mut mgr;
        // Start a transfer with size 0, then immediately complete it.
        sink.handle_file_start(&unique_id, "x.bin", 0).unwrap();
        sink.handle_file_done(&unique_id).unwrap();
        // File should be in Downloads.
        let dest = dir.join("Downloads").join("x.bin");
        assert!(dest.exists(), "expected file at {dest:?}");
        drop(mgr);
        fs::remove_dir_all(&dir).ok();
    }
}
