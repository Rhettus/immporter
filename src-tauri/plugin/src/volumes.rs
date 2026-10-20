//! Mounted volumes that could hold camera media.

use std::path::Path;

use serde::Serialize;
use sysinfo::Disks;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Volume {
    pub name: String,
    pub mount_point: String,
    pub total: u64,
    pub available: u64,
    pub removable: bool,
    pub file_system: String,
    /// The volume has a DCIM folder (camera card).
    pub has_dcim: bool,
}

fn is_candidate(mount: &Path) -> bool {
    let m = mount.to_string_lossy();
    #[cfg(target_os = "macos")]
    {
        m.starts_with("/Volumes/") && !m.contains("com.apple") && !m.starts_with("/Volumes/Recovery")
    }
    #[cfg(target_os = "linux")]
    {
        ["/media/", "/run/media/", "/mnt/"].iter().any(|p| m.starts_with(p))
    }
    #[cfg(target_os = "windows")]
    {
        // Every drive letter except the system drive.
        let sys = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
        !m.to_ascii_uppercase().starts_with(&sys.to_ascii_uppercase())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        !m.is_empty()
    }
}

pub fn list() -> Vec<Volume> {
    let disks = Disks::new_with_refreshed_list();
    let mut out: Vec<Volume> = disks
        .list()
        .iter()
        .filter(|d| is_candidate(d.mount_point()))
        .map(|d| {
            let mp = d.mount_point();
            let name = d.name().to_string_lossy().to_string();
            let name = if name.is_empty() {
                mp.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| mp.to_string_lossy().to_string())
            } else {
                name
            };
            Volume {
                name,
                mount_point: mp.to_string_lossy().to_string(),
                total: d.total_space(),
                available: d.available_space(),
                removable: d.is_removable(),
                file_system: d.file_system().to_string_lossy().to_string(),
                has_dcim: mp.join("DCIM").is_dir(),
            }
        })
        .collect();
    // Camera cards first, then removable, then by name.
    out.sort_by(|a, b| {
        (!a.has_dcim, !a.removable, a.name.to_lowercase()).cmp(&(!b.has_dcim, !b.removable, b.name.to_lowercase()))
    });
    out.dedup_by(|a, b| a.mount_point == b.mount_point);
    out
}
