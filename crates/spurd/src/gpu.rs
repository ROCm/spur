use spur_core::resource::{GpuLinkType, GpuResource};
use std::path::Path;
use tracing::debug;

/// Discover GPUs from sysfs (works for both AMD and NVIDIA).
pub fn discover_gpus() -> Vec<GpuResource> {
    let mut gpus = Vec::new();

    // Try AMD GPUs first (ROCm / amdgpu driver)
    gpus.extend(discover_amd_gpus());

    // Then NVIDIA GPUs
    gpus.extend(discover_nvidia_gpus());

    gpus
}

/// Discover AMD GPUs via /sys/class/drm/ and ROCm sysfs.
fn discover_amd_gpus() -> Vec<GpuResource> {
    let mut gpus = Vec::new();
    let drm_dir = Path::new("/sys/class/drm");

    if !drm_dir.exists() {
        return gpus;
    }

    let Ok(entries) = std::fs::read_dir(drm_dir) else {
        return gpus;
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        // Match card0, card1, etc. (skip renderD* nodes)
        if !name_str.starts_with("card") || name_str.contains("render") {
            continue;
        }
        // Skip entries like card0-DP-1
        if name_str.contains('-') {
            continue;
        }

        let card_path = entry.path();
        let device_path = card_path.join("device");

        // Check it's an AMD GPU
        let vendor = read_sysfs_string(&device_path.join("vendor"));
        if vendor.as_deref() != Some("0x1002") {
            continue; // Not AMD
        }

        let device_id: u32 = name_str
            .trim_start_matches("card")
            .parse()
            .unwrap_or(gpus.len() as u32);

        // Read GPU type from marketing name or device ID
        let gpu_type = detect_amd_gpu_type(&device_path);

        // Read VRAM size
        let memory_mb = read_amd_vram_mb(&device_path);

        // Detect xGMI links
        let (peers, link_type) = detect_amd_topology(&device_path, device_id);

        debug!(
            device_id,
            gpu_type = %gpu_type,
            memory_mb,
            peers = ?peers,
            "discovered AMD GPU"
        );

        gpus.push(GpuResource {
            device_id,
            gpu_type,
            memory_mb,
            peer_gpus: peers,
            link_type,
        });
    }

    gpus
}

fn detect_amd_gpu_type(device_path: &Path) -> String {
    // Try ROCm-specific marketing name
    if let Some(name) = read_sysfs_string(&device_path.join("product_name")) {
        return normalize_gpu_name(&name);
    }

    // Try hwmon name
    let hwmon_dir = device_path.join("hwmon");
    if let Ok(entries) = std::fs::read_dir(&hwmon_dir) {
        for entry in entries.flatten() {
            if let Some(name) = read_sysfs_string(&entry.path().join("name")) {
                if name != "amdgpu" {
                    return normalize_gpu_name(&name);
                }
            }
        }
    }

    // Fallback: read PCI device ID and map to name
    if let Some(dev_id) = read_sysfs_string(&device_path.join("device")) {
        return match dev_id.trim() {
            "0x74a1" | "0x74a0" => "mi300x".into(),
            "0x74b1" | "0x74b0" => "mi300a".into(),
            "0x7460" => "mi325x".into(),
            "0x150e" | "0x150d" => "rx9070xt".into(),
            "0x1506" | "0x1505" => "rx9070".into(),
            _ => format!("amdgpu-{}", dev_id.trim()),
        };
    }

    "amdgpu".into()
}

fn normalize_gpu_name(name: &str) -> String {
    let lower = name.to_lowercase();
    if lower.contains("mi300x") {
        "mi300x".into()
    } else if lower.contains("mi300a") {
        "mi300a".into()
    } else if lower.contains("mi325") {
        "mi325x".into()
    } else if lower.contains("mi250") {
        "mi250x".into()
    } else if lower.contains("mi210") {
        "mi210".into()
    } else if lower.contains("mi100") {
        "mi100".into()
    } else if lower.contains("9070 xt") || lower.contains("9070xt") {
        "rx9070xt".into()
    } else if lower.contains("9070") {
        "rx9070".into()
    } else {
        name.trim().to_lowercase().replace(' ', "-")
    }
}

fn read_amd_vram_mb(device_path: &Path) -> u64 {
    // Try mem_info_vram_total (bytes)
    if let Some(bytes_str) = read_sysfs_string(&device_path.join("mem_info_vram_total")) {
        if let Ok(bytes) = bytes_str.trim().parse::<u64>() {
            return bytes / (1024 * 1024);
        }
    }

    // Try gpu_mem (ROCm SMI)
    if let Some(mem_str) = read_sysfs_string(&device_path.join("gpu_mem")) {
        if let Ok(mb) = mem_str.trim().parse::<u64>() {
            return mb;
        }
    }

    0
}

fn detect_amd_topology(device_path: &Path, _device_id: u32) -> (Vec<u32>, GpuLinkType) {
    // Check for xGMI links via /sys/class/drm/cardN/device/amdgpu_xgmi_*
    let xgmi_hive = device_path.join("xgmi_hive_info");
    if xgmi_hive.exists() {
        // Has xGMI — discover peers
        // In practice, all GPUs in the same xGMI hive are peers
        // For now, return empty peers (requires cross-GPU enumeration)
        return (Vec::new(), GpuLinkType::XGMI);
    }

    (Vec::new(), GpuLinkType::PCIe)
}

/// Discover NVIDIA GPUs via sysfs.
fn discover_nvidia_gpus() -> Vec<GpuResource> {
    let mut gpus = Vec::new();
    let drm_dir = Path::new("/sys/class/drm");

    if !drm_dir.exists() {
        return gpus;
    }

    let Ok(entries) = std::fs::read_dir(drm_dir) else {
        return gpus;
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if !name_str.starts_with("card") || name_str.contains('-') {
            continue;
        }

        let device_path = entry.path().join("device");
        let vendor = read_sysfs_string(&device_path.join("vendor"));
        if vendor.as_deref() != Some("0x10de") {
            continue; // Not NVIDIA
        }

        let device_id: u32 = name_str
            .trim_start_matches("card")
            .parse()
            .unwrap_or(gpus.len() as u32);

        let gpu_type = detect_nvidia_gpu_type(&device_path);

        debug!(device_id, gpu_type = %gpu_type, "discovered NVIDIA GPU");

        gpus.push(GpuResource {
            device_id,
            gpu_type,
            memory_mb: 0, // Need nvidia-smi for memory info
            peer_gpus: Vec::new(),
            link_type: GpuLinkType::PCIe,
        });
    }

    gpus
}

fn detect_nvidia_gpu_type(device_path: &Path) -> String {
    if let Some(dev_id) = read_sysfs_string(&device_path.join("device")) {
        return match dev_id.trim() {
            "0x2330" | "0x2331" => "h100".into(),
            "0x2335" | "0x2336" | "0x2339" => "h200".into(),
            "0x2900" | "0x2901" => "b200".into(),
            "0x20b0" | "0x20b2" => "a100".into(),
            _ => format!("nvidia-{}", dev_id.trim()),
        };
    }
    "nvidia".into()
}

fn read_sysfs_string(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_gpu_name() {
        assert_eq!(normalize_gpu_name("AMD Instinct MI300X"), "mi300x");
        assert_eq!(normalize_gpu_name("AMD Radeon RX 9070 XT"), "rx9070xt");
        assert_eq!(normalize_gpu_name("mi250x"), "mi250x");
    }

    #[test]
    fn test_discover_gpus_does_not_panic() {
        // Should work on any system, just might return empty
        let gpus = discover_gpus();
        // No assertion on count — depends on hardware
        let _ = gpus;
    }
}
