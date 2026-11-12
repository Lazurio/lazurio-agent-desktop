//! The few places where macOS and Windows differ for the core.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

/// `macos` or `windows`, as the contract's `os` parameter expects.
pub fn os_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "linux"
    }
}

/// The system's version, for example `15.1.0` on macOS or `10.0.26100` on Windows.
pub fn os_version() -> String {
    os_info::get().version().to_string()
}

/// The system OpenSSH client the contract names.
pub fn default_ssh_program() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        let windir = std::env::var_os("WINDIR").unwrap_or_else(|| "C:\\Windows".into());
        PathBuf::from(windir).join("System32\\OpenSSH\\ssh.exe")
    }
    #[cfg(not(target_os = "windows"))]
    {
        PathBuf::from("/usr/bin/ssh")
    }
}

/// The name people know the computer by, as the system shows it: on macOS
/// the Computer Name from Sharing settings (for example "MacBook Pro"), on
/// Windows the device name. Never the user's login name.
pub fn computer_name() -> String {
    #[cfg(target_os = "macos")]
    if let Some(name) = macos::computer_name() {
        return name;
    }
    let host = gethostname::gethostname().to_string_lossy().to_string();
    let host = host.strip_suffix(".local").unwrap_or(&host).to_string();
    if host.is_empty() {
        "Computer".to_string()
    } else {
        host
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use core_foundation::base::TCFType;
    use core_foundation::string::{CFString, CFStringRef};
    use std::ffi::c_void;

    #[link(name = "SystemConfiguration", kind = "framework")]
    extern "C" {
        fn SCDynamicStoreCopyComputerName(store: *const c_void, encoding: *mut u32) -> CFStringRef;
    }

    pub(super) fn computer_name() -> Option<String> {
        // SAFETY: a null store asks SystemConfiguration for a temporary
        // session; the returned string follows the Create rule.
        let raw = unsafe { SCDynamicStoreCopyComputerName(std::ptr::null(), std::ptr::null_mut()) };
        if raw.is_null() {
            return None;
        }
        let name = unsafe { CFString::wrap_under_create_rule(raw) }.to_string();
        crate::pairing::display_name(&name)
    }
}

/// Child processes of the app never open a console window on Windows.
pub fn hide_console(cmd: &mut Command) -> &mut Command {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// Creates a new file that only the current user can read.
///
/// OpenSSH refuses a private key that others can read, on Windows too, so the
/// Windows branch removes inherited access and grants the user alone.
pub fn write_private_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let _ = std::fs::remove_file(path);
    #[cfg(unix)]
    let mut file = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?
    };
    #[cfg(not(unix))]
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    #[cfg(target_os = "windows")]
    restrict_to_current_user(path)?;
    file.write_all(contents)?;
    file.sync_all()
}

#[cfg(target_os = "windows")]
fn restrict_to_current_user(path: &Path) -> std::io::Result<()> {
    let user =
        std::env::var("USERNAME").map_err(|_| std::io::Error::other("USERNAME is not set"))?;
    let domain = std::env::var("USERDOMAIN").unwrap_or_default();
    let principal = if domain.is_empty() {
        user
    } else {
        format!("{domain}\\{user}")
    };
    let status = hide_console(&mut Command::new("icacls"))
        .arg(path)
        .args(["/inheritance:r", "/grant:r"])
        .arg(format!("{principal}:F"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(
            "icacls could not restrict the key file",
        ))
    }
}

/// Writes `contents` next to `path` and renames it over, so readers never see half a file.
pub fn write_atomic(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(contents)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}
