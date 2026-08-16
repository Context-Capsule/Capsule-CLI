#[cfg(not(windows))]
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemInfo {
    pub platform: String,
    pub version: Option<String>,
    pub architecture: String,
}

pub fn discover() -> SystemInfo {
    SystemInfo {
        platform: std::env::consts::OS.to_owned(),
        version: platform_version(),
        architecture: std::env::consts::ARCH.to_owned(),
    }
}

#[cfg(windows)]
#[repr(C)]
struct RtlOsVersionInfoW {
    size: u32,
    major_version: u32,
    minor_version: u32,
    build_number: u32,
    platform_id: u32,
    service_pack: [u16; 128],
}

#[cfg(windows)]
#[link(name = "ntdll")]
unsafe extern "system" {
    fn RtlGetVersion(version_information: *mut RtlOsVersionInfoW) -> i32;
}

#[cfg(windows)]
fn platform_version() -> Option<String> {
    let mut info = RtlOsVersionInfoW {
        size: std::mem::size_of::<RtlOsVersionInfoW>() as u32,
        major_version: 0,
        minor_version: 0,
        build_number: 0,
        platform_id: 0,
        service_pack: [0; 128],
    };

    let status = unsafe { RtlGetVersion(&mut info) };
    if status < 0 {
        return None;
    }

    Some(format_windows_version(
        info.major_version,
        info.minor_version,
        info.build_number,
    ))
}

#[cfg(windows)]
fn format_windows_version(major: u32, minor: u32, build: u32) -> String {
    format!("{major}.{minor}.{build}")
}

#[cfg(not(windows))]
fn platform_version() -> Option<String> {
    let output = Command::new("uname").arg("-r").output().ok()?;
    if !output.status.success() {
        return None;
    }

    let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!value.is_empty()).then_some(value)
}

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    use super::format_windows_version;

    #[cfg(windows)]
    #[test]
    fn windows_version_keeps_build_number() {
        assert_eq!(format_windows_version(10, 0, 26100), "10.0.26100");
    }
}
