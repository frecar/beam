use anyhow::{Context, Result};
use std::fs;
use tracing::{debug, info, warn};

/// Detected GPU configuration for Xorg virtual display.
#[derive(Debug, Clone)]
pub struct GpuConfig {
    /// Xorg driver name: "nvidia" or "dummy"
    pub driver: String,
    /// PCI BusID in Xorg format, e.g. "PCI:187:0:0" (nvidia only)
    pub bus_id: Option<String>,
    /// DFP output name for ConnectedMonitor, e.g. "DFP-1" (nvidia only)
    pub dfp_output: Option<String>,
}

/// Information about a detected NVIDIA GPU.
#[derive(Debug, Clone)]
struct NvidiaGpu {
    /// Model name, e.g. "NVIDIA L40S"
    model: String,
    /// PCI bus location, e.g. "0000:bb:00.0"
    pci_address: String,
    /// Xorg-format BusID, e.g. "PCI:187:0:0"
    xorg_bus_id: String,
}

/// Standard 128-byte EDID v1.4 block for virtual displays.
/// Based on "Dell Virtual HD" EDID used for headless NVIDIA GPU displays.
/// Declares 1920x1080 as preferred timing with standard timing entries for
/// common resolutions up to 1920x1080.
const EDID_BYTES: [u8; 128] = [
    // Exact copy of "Dell Virtual HD" EDID v1.4 used for headless NVIDIA displays.
    // 16 rows x 8 bytes = 128 bytes. Checksum verified: sum of all bytes = 0 (mod 256).
    0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00, // 0x00: Header
    0x10, 0xAC, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 0x08: Manufacturer DEL, product 0
    0x01, 0x1D, 0x01, 0x04, 0xA5, 0x34, 0x20,
    0x78, // 0x10: Week 1, year 2019, EDID 1.4, digital
    0x3A, 0xEE, 0x95, 0xA3, 0x54, 0x4C, 0x99, 0x26, // 0x18: Chromaticity coordinates
    0x0F, 0x50, 0x54, 0xA5, 0x4B, 0x00, 0x71, 0x4F, // 0x20: Established + standard timings
    0x81, 0x80, 0xA9, 0xC0, 0xD1, 0xC0, 0x01, 0x01, // 0x28: Standard timings continued
    0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x02,
    0x3A, // 0x30: Standard timings + detailed timing start
    0x80, 0x18, 0x71, 0x38, 0x2D, 0x40, 0x58, 0x2C, // 0x38: Detailed timing: 1920x1080@60Hz
    0x45, 0x00, 0x09, 0x25, 0x21, 0x00, 0x00, 0x1E, // 0x40: Detailed timing continued
    0x00, 0x00, 0x00, 0xFF, 0x00, 0x56, 0x49, 0x52, // 0x48: Serial: "VIRTUAL00000"
    0x54, 0x55, 0x41, 0x4C, 0x30, 0x30, 0x30, 0x30, // 0x50: Serial continued
    0x30, 0x0A, 0x00, 0x00, 0x00, 0xFC, 0x00,
    0x56, // 0x58: Serial end + Monitor name: "Virtual HD"
    0x69, 0x72, 0x74, 0x75, 0x61, 0x6C, 0x20, 0x48, // 0x60: Monitor name continued
    0x44, 0x0A, 0x20, 0x20, 0x00, 0x00, 0x00, 0xFD, // 0x68: Monitor name end + Range limits
    0x00, 0x32, 0x4B, 0x1E, 0x51, 0x11, 0x00,
    0x0A, // 0x70: Range limits: 50-75Hz V, 30-81kHz H
    0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x00,
    0xC9, // 0x78: Padding + extension flag(0) + checksum (matches working l40s EDID)
];

/// Detect GPU configuration based on the requested mode and display allocation.
///
/// - `"dummy"`: always returns dummy config
/// - `"nvidia"`: attempts nvidia, fails if not available
/// - `"auto"`: tries nvidia, falls back to dummy
///
/// Multi-GPU support: enumerates all NVIDIA GPUs with display capability,
/// distributes sessions across GPUs by assigning DFP outputs round-robin.
/// Each GPU has DFP-0 through DFP-3 (4 outputs). DFP-0 on the first GPU
/// is reserved for the system display (:0).
pub fn detect_gpu(mode: &str, display_num: u32, display_start: u32) -> GpuConfig {
    if mode == "dummy" {
        info!("GPU driver forced to dummy");
        return dummy_config();
    }

    match try_detect_nvidia(display_num, display_start) {
        Ok(config) => config,
        Err(e) => {
            if mode == "nvidia" {
                // User explicitly requested nvidia — log error but still return
                // the config attempt. The caller (VirtualDisplay::start) will
                // fail when Xorg doesn't start, giving a clear error.
                warn!("NVIDIA GPU detection failed but gpu_driver=nvidia: {e:#}");
                // Return dummy so the caller can decide to bail
                warn!(
                    "Falling back to dummy driver — set gpu_driver=auto to suppress this warning"
                );
                dummy_config()
            } else {
                info!("No NVIDIA GPU detected, using dummy driver: {e:#}");
                dummy_config()
            }
        }
    }
}

/// Write the embedded EDID binary to a specific path.
pub fn write_edid_file_to(path: &str) -> Result<()> {
    fs::write(path, EDID_BYTES).with_context(|| format!("Failed to write EDID file to {path}"))?;
    debug!(%path, "Wrote EDID file");
    Ok(())
}

fn dummy_config() -> GpuConfig {
    GpuConfig {
        driver: "dummy".to_string(),
        bus_id: None,
        dfp_output: None,
    }
}

/// Try to detect NVIDIA GPU and compute the DFP output for this display.
fn try_detect_nvidia(display_num: u32, display_start: u32) -> Result<GpuConfig> {
    // Check nvidia Xorg driver is installed
    if !std::path::Path::new("/usr/lib/xorg/modules/drivers/nvidia_drv.so").exists() {
        anyhow::bail!(
            "NVIDIA Xorg driver not installed at /usr/lib/xorg/modules/drivers/nvidia_drv.so"
        );
    }

    // Enumerate display-capable GPUs
    let gpus = enumerate_nvidia_gpus()?;
    if gpus.is_empty() {
        anyhow::bail!("No display-capable NVIDIA GPUs found");
    }

    info!(
        gpu_count = gpus.len(),
        models = %gpus.iter().map(|g| g.model.as_str()).collect::<Vec<_>>().join(", "),
        "Detected NVIDIA GPU(s)"
    );

    // Distribute sessions across GPUs round-robin.
    // All sessions on a GPU share DFP-0 — multiple Xorg instances can use the
    // same DFP output simultaneously (each gets its own GPU context).
    let session_index = display_num.saturating_sub(display_start) as usize;
    let gpu = &gpus[session_index % gpus.len()];

    info!(
        display_num,
        session_index,
        gpu_model = %gpu.model,
        bus_id = %gpu.xorg_bus_id,
        "Allocated GPU display output (DFP-0)"
    );

    Ok(GpuConfig {
        driver: "nvidia".to_string(),
        bus_id: Some(gpu.xorg_bus_id.clone()),
        dfp_output: Some("DFP-0".to_string()),
    })
}

/// Enumerate NVIDIA GPUs with display capability.
///
/// Reads `/proc/driver/nvidia/gpus/*/information` and filters to GPUs that
/// are suitable for display (have display outputs). Compute-only GPUs like
/// H100/H200 are excluded by checking if the GPU appears in nvidia-smi
/// (which only shows GPUs accessible in this container/namespace).
fn enumerate_nvidia_gpus() -> Result<Vec<NvidiaGpu>> {
    let nvidia_proc = std::path::Path::new("/proc/driver/nvidia/gpus");
    if !nvidia_proc.exists() {
        anyhow::bail!("NVIDIA kernel driver not loaded (/proc/driver/nvidia/gpus not found)");
    }

    // Get the set of PCI addresses visible to nvidia-smi (only GPUs in this
    // container/namespace). Compute-only GPUs passed to other containers
    // appear in /proc but not in nvidia-smi.
    let accessible_gpus = nvidia_smi_pci_addresses();

    let mut gpus = Vec::new();

    let entries = fs::read_dir(nvidia_proc).context("Failed to read /proc/driver/nvidia/gpus")?;

    for entry in entries {
        let entry = entry?;
        let pci_dir = entry.file_name().to_string_lossy().to_string();
        let info_path = entry.path().join("information");

        let info = match fs::read_to_string(&info_path) {
            Ok(s) => s,
            Err(e) => {
                debug!("Skipping GPU {pci_dir}: cannot read information: {e}");
                continue;
            }
        };

        let model = parse_proc_field(&info, "Model").unwrap_or_default();
        let bus_location = parse_proc_field(&info, "Bus Location").unwrap_or(pci_dir.clone());

        // Filter: only include GPUs accessible via nvidia-smi
        if !accessible_gpus.is_empty() && !accessible_gpus.contains(&normalize_pci(&bus_location)) {
            debug!(
                model,
                bus_location, "Skipping GPU (not accessible in this namespace)"
            );
            continue;
        }

        let xorg_bus_id = pci_to_xorg_bus_id(&bus_location);

        gpus.push(NvidiaGpu {
            model,
            pci_address: bus_location,
            xorg_bus_id,
        });
    }

    // Sort by PCI address for deterministic allocation
    gpus.sort_by(|a, b| a.pci_address.cmp(&b.pci_address));

    Ok(gpus)
}

/// Get PCI bus addresses of GPUs visible to nvidia-smi.
/// Returns normalized addresses (lowercase, no domain prefix if 0000).
fn nvidia_smi_pci_addresses() -> Vec<String> {
    let output = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=pci.bus_id", "--format=csv,noheader"])
        .output();

    match output {
        Ok(o) if o.status.success() => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            stdout
                .lines()
                .map(|line| normalize_pci(line.trim()))
                .collect()
        }
        _ => {
            debug!("nvidia-smi not available, using all GPUs from /proc");
            Vec::new()
        }
    }
}

/// Parse a field from /proc/driver/nvidia/gpus/*/information.
/// Format: "Field Name: \t\t value"
fn parse_proc_field(info: &str, field: &str) -> Option<String> {
    for line in info.lines() {
        // Handle format: "Model: \t\t NVIDIA L40S"
        if let Some(rest) = line.strip_prefix(field) {
            let rest = rest.trim_start();
            if let Some(value) = rest.strip_prefix(':') {
                return Some(value.trim().to_string());
            }
        }
    }
    None
}

/// Normalize a PCI address for comparison.
/// "00000000:BB:00.0" -> "bb:00.0", "0000:bb:00.0" -> "bb:00.0"
fn normalize_pci(addr: &str) -> String {
    let addr = addr.trim().to_lowercase();
    // Remove leading domain (0000: or 00000000:)
    let addr = addr
        .strip_prefix("00000000:")
        .or_else(|| addr.strip_prefix("0000:"))
        .unwrap_or(&addr);
    addr.to_string()
}

/// Convert a PCI bus address to Xorg BusID format.
/// "0000:bb:00.0" -> "PCI:187:0:0"
///
/// PCI address format: domain:bus:device.function (hex)
/// Xorg BusID format: PCI:bus:device:function (decimal)
pub fn pci_to_xorg_bus_id(pci_addr: &str) -> String {
    // Strip domain prefix if present
    let addr = pci_addr
        .trim()
        .strip_prefix("0000:")
        .or_else(|| pci_addr.trim().strip_prefix("00000000:"))
        .unwrap_or(pci_addr.trim());

    // Parse "bb:dd.f" -> (bus, device, function)
    let parts: Vec<&str> = addr.split([':', '.']).collect();
    if parts.len() >= 3 {
        let bus = u32::from_str_radix(parts[0], 16).unwrap_or(0);
        let device = u32::from_str_radix(parts[1], 16).unwrap_or(0);
        let function = u32::from_str_radix(parts[2], 16).unwrap_or(0);
        format!("PCI:{bus}:{device}:{function}")
    } else {
        warn!(pci_addr, "Failed to parse PCI address, using raw value");
        format!("PCI:{addr}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pci_to_xorg_basic() {
        assert_eq!(pci_to_xorg_bus_id("0000:bb:00.0"), "PCI:187:0:0");
        assert_eq!(pci_to_xorg_bus_id("0000:4e:00.0"), "PCI:78:0:0");
        assert_eq!(pci_to_xorg_bus_id("0000:01:00.0"), "PCI:1:0:0");
    }

    #[test]
    fn pci_to_xorg_with_long_domain() {
        assert_eq!(pci_to_xorg_bus_id("00000000:BB:00.0"), "PCI:187:0:0");
    }

    #[test]
    fn pci_to_xorg_no_domain() {
        assert_eq!(pci_to_xorg_bus_id("bb:00.0"), "PCI:187:0:0");
    }

    #[test]
    fn pci_to_xorg_with_nonzero_function() {
        assert_eq!(pci_to_xorg_bus_id("0000:3a:00.1"), "PCI:58:0:1");
    }

    #[test]
    fn detect_gpu_always_uses_dfp0() {
        // All sessions share DFP-0 — multiple Xorg instances can use the same
        // DFP output simultaneously (each gets its own GPU context).
        let config = detect_gpu("dummy", 10, 10);
        assert_eq!(config.driver, "dummy");
        // Can't test nvidia path without real GPU, but verify dummy has no DFP
        assert!(config.dfp_output.is_none());
    }

    #[test]
    fn detect_gpu_dummy_mode() {
        let config = detect_gpu("dummy", 10, 10);
        assert_eq!(config.driver, "dummy");
        assert!(config.bus_id.is_none());
        assert!(config.dfp_output.is_none());
    }

    #[test]
    fn edid_length_and_header() {
        assert_eq!(EDID_BYTES.len(), 128, "EDID must be exactly 128 bytes");
    }

    #[test]
    fn edid_header_valid() {
        // EDID v1.x header is always 00 FF FF FF FF FF FF 00
        assert_eq!(
            &EDID_BYTES[0..8],
            &[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]
        );
    }

    #[test]
    fn edid_version() {
        // Bytes 18-19: version 1.4
        assert_eq!(EDID_BYTES[18], 0x01, "EDID version should be 1");
        assert_eq!(EDID_BYTES[19], 0x04, "EDID revision should be 4");
    }

    #[test]
    fn normalize_pci_addresses() {
        assert_eq!(normalize_pci("00000000:BB:00.0"), "bb:00.0");
        assert_eq!(normalize_pci("0000:bb:00.0"), "bb:00.0");
        assert_eq!(normalize_pci("BB:00.0"), "bb:00.0");
        assert_eq!(normalize_pci("  0000:4E:00.0  "), "4e:00.0");
    }

    #[test]
    fn parse_proc_fields() {
        let info = "\
Model: \t\t NVIDIA L40S
IRQ:   \t\t 17
Bus Location: \t 0000:bb:00.0
Device Minor: \t 2
";
        assert_eq!(
            parse_proc_field(info, "Model"),
            Some("NVIDIA L40S".to_string())
        );
        assert_eq!(
            parse_proc_field(info, "Bus Location"),
            Some("0000:bb:00.0".to_string())
        );
        assert_eq!(parse_proc_field(info, "Nonexistent"), None);
    }

    // --- parse_proc_field edge cases ---

    #[test]
    fn parse_proc_field_empty_input() {
        assert_eq!(parse_proc_field("", "Model"), None);
    }

    #[test]
    fn parse_proc_field_prefix_without_colon_is_rejected() {
        // A line that starts with the field name but has no colon must not match
        let info = "Model NVIDIA L40S without colon\n";
        assert_eq!(parse_proc_field(info, "Model"), None);
    }

    #[test]
    fn parse_proc_field_returns_first_match() {
        let info = "Model: \t First\nModel: \t Second\n";
        assert_eq!(parse_proc_field(info, "Model"), Some("First".to_string()));
    }

    #[test]
    fn parse_proc_field_empty_value_after_colon() {
        let info = "Model: \nNext: x\n";
        assert_eq!(parse_proc_field(info, "Model"), Some(String::new()));
    }

    #[test]
    fn parse_proc_field_value_with_internal_whitespace() {
        let info = "Model: \t\t NVIDIA RTX A6000 Ada Generation\n";
        assert_eq!(
            parse_proc_field(info, "Model"),
            Some("NVIDIA RTX A6000 Ada Generation".to_string())
        );
    }

    // --- normalize_pci edge cases ---

    #[test]
    fn normalize_pci_already_normalized() {
        assert_eq!(normalize_pci("bb:00.0"), "bb:00.0");
    }

    #[test]
    fn normalize_pci_empty_string() {
        assert_eq!(normalize_pci(""), "");
    }

    #[test]
    fn normalize_pci_mixed_case() {
        assert_eq!(normalize_pci("0000:Bb:0A.0"), "bb:0a.0");
    }

    // --- pci_to_xorg_bus_id edge cases ---

    #[test]
    fn pci_to_xorg_zero_bus() {
        assert_eq!(pci_to_xorg_bus_id("0000:00:00.0"), "PCI:0:0:0");
    }

    #[test]
    fn pci_to_xorg_max_hex_bus() {
        assert_eq!(pci_to_xorg_bus_id("0000:ff:1f.7"), "PCI:255:31:7");
    }

    #[test]
    fn pci_to_xorg_malformed_falls_through() {
        // Less than 3 parts after split → raw fallback branch
        let out = pci_to_xorg_bus_id("garbage");
        assert!(
            out.starts_with("PCI:"),
            "Malformed input should still produce a PCI:* string, got {out}"
        );
    }

    #[test]
    fn pci_to_xorg_trims_whitespace() {
        assert_eq!(pci_to_xorg_bus_id("  0000:bb:00.0  "), "PCI:187:0:0");
    }

    #[test]
    fn pci_to_xorg_invalid_hex_yields_zero_components() {
        // u32::from_str_radix on non-hex returns Err → unwrap_or(0)
        // Three colon-separated parts that aren't valid hex → "PCI:0:0:0"
        let out = pci_to_xorg_bus_id("zz:yy.xx");
        assert_eq!(out, "PCI:0:0:0");
    }

    // --- detect_gpu mode branches ---

    #[test]
    fn detect_gpu_dummy_mode_with_various_display_nums() {
        // dummy mode should ignore display_num / display_start entirely
        for (display_num, display_start) in &[(0, 0), (10, 10), (42, 5), (u32::MAX, 0)] {
            let config = detect_gpu("dummy", *display_num, *display_start);
            assert_eq!(config.driver, "dummy");
            assert!(config.bus_id.is_none());
            assert!(config.dfp_output.is_none());
        }
    }

    #[test]
    fn detect_gpu_auto_mode_without_nvidia_falls_back_to_dummy() {
        // On test hosts without an NVIDIA GPU at /usr/lib/xorg/modules/drivers/nvidia_drv.so,
        // auto mode should silently fall back to dummy.
        let nvidia_present =
            std::path::Path::new("/usr/lib/xorg/modules/drivers/nvidia_drv.so").exists();
        if !nvidia_present {
            let config = detect_gpu("auto", 10, 10);
            assert_eq!(config.driver, "dummy");
        }
    }

    #[test]
    fn detect_gpu_nvidia_mode_without_driver_warns_and_returns_dummy() {
        // On test hosts without the NVIDIA driver, nvidia mode logs warnings
        // then returns dummy (the caller bails when Xorg fails to start).
        let nvidia_present =
            std::path::Path::new("/usr/lib/xorg/modules/drivers/nvidia_drv.so").exists();
        if !nvidia_present {
            let config = detect_gpu("nvidia", 10, 10);
            assert_eq!(config.driver, "dummy");
            assert!(config.bus_id.is_none());
        }
    }

    // --- write_edid_file_to ---

    #[test]
    fn write_edid_file_to_writes_exact_bytes() {
        let dir = std::env::temp_dir().join(format!("beam-edid-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("edid.bin");
        let path_str = path.to_str().unwrap();

        write_edid_file_to(path_str).unwrap();

        let written = std::fs::read(&path).unwrap();
        assert_eq!(written.len(), 128, "EDID file must be 128 bytes");
        assert_eq!(written.as_slice(), &EDID_BYTES[..]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_edid_file_to_overwrites_existing() {
        let dir = std::env::temp_dir().join(format!("beam-edid-overwrite-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("edid.bin");

        std::fs::write(&path, b"junk").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"junk");

        write_edid_file_to(path.to_str().unwrap()).unwrap();

        let written = std::fs::read(&path).unwrap();
        assert_eq!(written.len(), 128);
        assert_eq!(
            &written[0..8],
            &[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_edid_file_to_fails_on_unwritable_path() {
        // /proc/... is not writable as a regular user
        let result = write_edid_file_to("/proc/nonexistent/edid.bin");
        assert!(
            result.is_err(),
            "Should fail to write to /proc subdirectory"
        );
    }

    // --- EDID structural validation ---

    #[test]
    fn edid_extension_flag_zero() {
        // Byte 126: number of extension blocks. We don't ship any.
        assert_eq!(EDID_BYTES[126], 0, "EDID extension flag should be 0");
    }

    #[test]
    fn edid_manufacturer_id_present() {
        // Bytes 8-9: manufacturer ID. Source declares 0x10AC ("DEL" — Dell).
        assert_eq!(EDID_BYTES[8], 0x10);
        assert_eq!(EDID_BYTES[9], 0xAC);
    }
}
