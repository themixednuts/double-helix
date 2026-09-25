//! Keeps a spawned process and everything it starts together.
//!
//! Agents usually run through a launcher (`npx.cmd` → `node.exe` → tools), and terminal
//! commands through `cmd.exe`. Killing only the direct child leaves the rest running, holding
//! ports and files. On Windows the child goes into a Job Object that kills the whole tree when
//! asked to, and when its last handle closes, which includes the editor exiting.

#[cfg(windows)]
mod imp {
    use std::os::windows::io::RawHandle;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    pub(crate) struct ProcessTree(HANDLE);

    // SAFETY: a job object handle may be used and closed from any thread.
    unsafe impl Send for ProcessTree {}
    unsafe impl Sync for ProcessTree {}

    impl ProcessTree {
        /// Put `process` (and whatever it starts from now on) into a new kill-on-close job.
        pub(crate) fn adopt(process: RawHandle) -> Option<Self> {
            // SAFETY: plain Win32 calls on handles we own; `info` outlives the call that
            // reads it, and the job handle is closed on every failure path.
            unsafe {
                let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if job.is_null() {
                    return None;
                }
                let tree = Self(job);
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let configured = SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    std::ptr::from_ref(&info).cast(),
                    std::mem::size_of_val(&info) as u32,
                ) != 0;
                (configured && AssignProcessToJobObject(job, process as HANDLE) != 0)
                    .then_some(tree)
            }
        }

        /// Kill every process in the tree.
        pub(crate) fn kill(&self) {
            // SAFETY: `self.0` is a live job handle until drop.
            unsafe {
                TerminateJobObject(self.0, 1);
            }
        }
    }

    impl Drop for ProcessTree {
        fn drop(&mut self) {
            // SAFETY: we own the handle. Kill-on-close ends anything still running.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    pub(crate) struct ProcessTree;

    impl ProcessTree {
        pub(crate) fn kill(&self) {}
    }
}

pub(crate) use imp::ProcessTree;

impl ProcessTree {
    /// Track the tree rooted at `child`, where the platform supports it.
    pub(crate) fn of(child: &tokio::process::Child) -> Option<Self> {
        #[cfg(windows)]
        {
            child.raw_handle().and_then(Self::adopt)
        }
        #[cfg(not(windows))]
        {
            let _ = child;
            None
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use crate::client::ProcessHandle;
    use std::time::Duration;

    /// A grandchild started in the background (`start /b`) outlives its parent `cmd.exe`,
    /// but not the handle: dropping it closes the job and ends the grandchild before it can
    /// write its marker.
    #[tokio::test]
    async fn dropping_the_handle_kills_background_grandchildren() {
        let dir = std::env::temp_dir().join(format!("acp-tree-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let marker = dir.join("marker.txt");
        let _ = std::fs::remove_file(&marker);

        let mut command = tokio::process::Command::new("cmd");
        command
            .arg("/D")
            .arg("/S")
            .arg("/C")
            .raw_arg(format!(
                "\"start /b cmd /D /C \"ping -n 4 127.0.0.1 >nul & echo late> {}\"\"",
                marker.display()
            ))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let child = command.spawn().expect("spawn cmd");
        let handle = ProcessHandle::spawn(child, "process-tree-test".to_owned());

        // The direct child exits at once; its background grandchild keeps running.
        tokio::time::timeout(Duration::from_secs(5), handle.waiter().wait())
            .await
            .expect("parent cmd exits promptly")
            .expect("parent exit status");
        drop(handle);

        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(
            !marker.exists(),
            "background grandchild survived the job being closed"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
