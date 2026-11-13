//! Ending the engine together with everything it started.
//!
//! The engine runs tool calls as child processes of itself and, on Windows,
//! keeps a detached cursor renderer (another `agent-desktop.exe`) for the
//! session. Killing only the engine leaves those running, and a running
//! `agent-desktop.exe` blocks an update from replacing the file. On Windows
//! the session's processes therefore live in a Job Object that is terminated
//! as a whole when the session ends (and closes with the app). Elsewhere this
//! is a no-op; macOS replaces the app bundle while its processes run.

use std::path::Path;
use std::process::Child;
use std::time::Duration;

pub struct ProcessTree {
    #[cfg(windows)]
    job: Option<windows::Job>,
}

impl ProcessTree {
    pub fn new() -> Self {
        Self {
            #[cfg(windows)]
            job: windows::Job::new(),
        }
    }

    /// Puts `child` and everything it starts from now on into the tree.
    pub fn adopt(&self, child: &Child) {
        #[cfg(windows)]
        if let Some(job) = &self.job {
            job.assign(child);
        }
        #[cfg(not(windows))]
        let _ = child;
    }

    /// Terminates every process of the tree and waits until none is left, at
    /// most `timeout`. Returns whether the tree is empty.
    pub fn end(&self, timeout: Duration) -> bool {
        #[cfg(windows)]
        if let Some(job) = &self.job {
            return job.terminate_and_wait(timeout);
        }
        let _ = timeout;
        true
    }
}

impl Default for ProcessTree {
    fn default() -> Self {
        Self::new()
    }
}

/// Terminates every process of this user whose executable lives in `dir`,
/// except the current process, and waits for each at most until `timeout`.
/// Windows only (an installer cannot replace a running `.exe`); returns how
/// many processes it ended.
pub fn end_processes_in(dir: &Path, timeout: Duration) -> usize {
    #[cfg(windows)]
    {
        windows::end_processes_in(dir, timeout)
    }
    #[cfg(not(windows))]
    {
        let _ = (dir, timeout);
        0
    }
}

#[cfg(windows)]
mod windows {
    use std::os::windows::ffi::OsStringExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;
    use std::process::Child;
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicAccountingInformation,
        JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
        TerminateJobObject, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
        PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
        PROCESS_TERMINATE,
    };

    pub(super) struct Job(HANDLE);

    // SAFETY: a job handle may be used from any thread.
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    impl Job {
        pub(super) fn new() -> Option<Self> {
            // SAFETY: plain Win32 calls on a handle owned here; the struct is
            // zero-initialized as the API expects.
            unsafe {
                let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if handle.is_null() {
                    return None;
                }
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let ok = SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const core::ffi::c_void,
                    std::mem::size_of_val(&info) as u32,
                );
                if ok == 0 {
                    CloseHandle(handle);
                    return None;
                }
                Some(Self(handle))
            }
        }

        pub(super) fn assign(&self, child: &Child) {
            // SAFETY: both handles are valid for the duration of the call.
            unsafe {
                AssignProcessToJobObject(self.0, child.as_raw_handle() as HANDLE);
            }
        }

        fn active_processes(&self) -> u32 {
            // SAFETY: the out struct is sized for this information class.
            unsafe {
                let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = std::mem::zeroed();
                let ok = QueryInformationJobObject(
                    self.0,
                    JobObjectBasicAccountingInformation,
                    &mut info as *mut _ as *mut core::ffi::c_void,
                    std::mem::size_of_val(&info) as u32,
                    std::ptr::null_mut(),
                );
                if ok == 0 {
                    0
                } else {
                    info.ActiveProcesses
                }
            }
        }

        pub(super) fn terminate_and_wait(&self, timeout: Duration) -> bool {
            // SAFETY: valid job handle.
            unsafe {
                TerminateJobObject(self.0, 1);
            }
            let deadline = Instant::now() + timeout;
            while self.active_processes() > 0 {
                if Instant::now() >= deadline {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            true
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: closing the handle we own; with KILL_ON_JOB_CLOSE this
            // also ends any process still in the job.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    fn image_path(process: HANDLE) -> Option<std::path::PathBuf> {
        let mut buf = vec![0u16; 32_768];
        let mut len = buf.len() as u32;
        // SAFETY: the buffer and its length match.
        let ok = unsafe {
            QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len)
        };
        (ok != 0).then(|| std::ffi::OsString::from_wide(&buf[..len as usize]).into())
    }

    fn starts_with_dir(path: &Path, dir: &Path) -> bool {
        let path = path.to_string_lossy().to_lowercase();
        let mut dir = dir.to_string_lossy().to_lowercase();
        if !dir.ends_with('\\') {
            dir.push('\\');
        }
        path.starts_with(&dir)
    }

    /// The folder in the long form Windows reports image paths in (no 8.3
    /// short names, no `\\?\` prefix).
    fn long_form(dir: &Path) -> std::path::PathBuf {
        std::fs::canonicalize(dir)
            .map(|p| {
                let s = p.to_string_lossy().to_string();
                std::path::PathBuf::from(s.strip_prefix(r"\\?\").unwrap_or(&s))
            })
            .unwrap_or_else(|_| dir.to_path_buf())
    }

    pub(super) fn end_processes_in(dir: &Path, timeout: Duration) -> usize {
        let dir = &long_form(dir);
        let me = std::process::id();
        let deadline = Instant::now() + timeout;
        let mut ended = 0;
        // SAFETY: a process snapshot walked with a correctly sized entry.
        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snapshot == INVALID_HANDLE_VALUE {
                return 0;
            }
            let mut entry: PROCESSENTRY32W = std::mem::zeroed();
            entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            let mut more = Process32FirstW(snapshot, &mut entry) != 0;
            while more {
                let pid = entry.th32ProcessID;
                if pid != me && pid != 0 {
                    let process = OpenProcess(
                        PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE | PROCESS_SYNCHRONIZE,
                        0,
                        pid,
                    );
                    if !process.is_null() {
                        if image_path(process).is_some_and(|p| starts_with_dir(&p, dir)) {
                            TerminateProcess(process, 1);
                            let left = deadline.saturating_duration_since(Instant::now());
                            WaitForSingleObject(
                                process,
                                left.as_millis().min(u32::MAX as u128) as u32,
                            );
                            ended += 1;
                        }
                        CloseHandle(process);
                    }
                }
                more = Process32NextW(snapshot, &mut entry) != 0;
            }
            CloseHandle(snapshot);
        }
        ended
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn ends_the_children_of_children_too() {
        let tree = ProcessTree::new();
        // cmd starts ping as its own child: the shape of engine → tool or renderer.
        let mut child = Command::new("cmd")
            .args(["/c", "ping -n 120 127.0.0.1 >NUL"])
            .spawn()
            .unwrap();
        tree.adopt(&child);
        std::thread::sleep(Duration::from_secs(2));
        assert!(
            tree.end(Duration::from_secs(10)),
            "a process of the tree survived"
        );
        assert!(child.wait().is_ok());
    }

    #[test]
    fn ends_leftovers_of_a_folder_but_not_itself() {
        let dir = std::env::temp_dir().join(format!("lad-tree-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ping = dir.join("leftover.exe");
        std::fs::copy(r"C:\Windows\System32\PING.EXE", &ping).unwrap();
        let mut leftover = Command::new(&ping)
            .args(["-n", "120", "127.0.0.1"])
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_secs(1));
        assert_eq!(end_processes_in(&dir, Duration::from_secs(10)), 1);
        assert!(leftover.wait().is_ok());
        assert_eq!(end_processes_in(&dir, Duration::from_secs(1)), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
