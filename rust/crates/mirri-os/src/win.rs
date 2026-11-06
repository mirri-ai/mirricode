//! Windows process-tree termination (batch A2 / #189).
//!
//! `PtyTerminal` on Windows previously killed only its direct child
//! (`TerminateProcess`), leaving grandchildren (e.g. a `start notepad.exe`
//! spawned inside the shell) orphaned. This module provides the two-tier
//! remedy:
//!
//! 1. [`JobHandle`] — a Job Object configured with
//!    `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`; when the handle is closed (or
//!    [`JobHandle::terminate`] is called), the kernel terminates every
//!    process still assigned to the job, grandchildren included.
//! 2. [`kill_process_tree`] — the fallback when job assignment failed:
//!    snapshot the process table, resolve the descendant tree via
//!    [`collect_descendants`], and terminate each process.
//!
//! The tree-walking logic ([`collect_descendants`]) is pure and compiled on
//! every platform, so its recursion and cycle safety are unit-tested off
//! Windows too.

/// Given `(pid, ppid)` pairs, return all pids in the tree rooted at
/// `root_pid` (the root included), in top-down order — parents before their
/// children, so callers terminating in list order hit owners before owned.
///
/// Cycle-safe: entries already visited are never revisited, so a corrupted
/// snapshot containing a `ppid` cycle cannot loop forever.
pub fn collect_descendants(procs: &[(u32, u32)], root_pid: u32) -> Vec<u32> {
    use std::collections::{HashMap, HashSet};

    let children_of: HashMap<u32, Vec<u32>> = {
        let mut map: HashMap<u32, Vec<u32>> = HashMap::new();
        for &(pid, ppid) in procs {
            map.entry(ppid).or_default().push(pid);
        }
        map
    };

    let mut visited: HashSet<u32> = HashSet::new();
    let mut ordered: Vec<u32> = Vec::new();
    let mut stack: Vec<u32> = vec![root_pid];
    while let Some(pid) = stack.pop() {
        if !visited.insert(pid) {
            continue;
        }
        ordered.push(pid);
        if let Some(children) = children_of.get(&pid) {
            // Push in reverse so the pop order keeps children in snapshot
            // order (cosmetic — the contract is only parents-before-children
            // within each root-to-leaf path).
            for &child in children.iter().rev() {
                if !visited.contains(&child) {
                    stack.push(child);
                }
            }
        }
    }
    ordered
}

/// The Windows Job Object that terminates its assigned processes when the
/// handle closes.
///
/// The handle is only ever closed (or explicitly terminated) — `Drop` never
/// terminates; only [`JobHandle::terminate`] (and closing the last handle
/// with `KILL_ON_JOB_CLOSE`) does.
#[cfg(windows)]
pub struct JobHandle {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
impl JobHandle {
    /// Create a Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` and
    /// assign the process `pid` to it.
    ///
    /// Grandchildren spawned *before* this assignment completes are NOT in
    /// the job — the Job Object only covers processes created after assign,
    /// there is no retroactive walk (that is exactly the window
    /// `kill_process_tree`'s toolhelp32 snapshot would cover, which we do
    /// not run when the job path succeeds). Known trade-off: the caller
    /// assigns the job as early as possible after spawn to shrink it.
    ///
    /// Returns `Err` if `pid` is the current process (a self-assignment
    /// guard — assigning our own process to a kill-on-close job would
    /// terminate the server when the handle drops), or if any Win32 call
    /// fails.
    pub fn assign(pid: u32) -> std::io::Result<Self> {
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        };
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
        };

        if pid == std::process::id() {
            return Err(std::io::Error::other(
                "refusing to assign the current process to a kill-on-close job",
            ));
        }

        // SAFETY: plain Win32 calls with no outstanding borrows; the returned
        // job handle is null-checked before use.
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            return Err(std::io::Error::last_os_error());
        }

        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: `job` is a valid job handle and `info` is fully initialized.
        let ok = unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &mut info as *mut _ as *const core::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            let err = std::io::Error::last_os_error();
            // SAFETY: we own `job` and will not use it again.
            unsafe { windows_sys::Win32::Foundation::CloseHandle(job) };
            return Err(err);
        }

        // Open with PROCESS_SET_QUOTA (AssignProcessToJobObject's requirement)
        // plus PROCESS_TERMINATE so the direct child can still be killed via
        // this handle if ever needed.
        // SAFETY: plain Win32 call with a numeric pid.
        let process = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid) };
        if process.is_null() {
            let err = std::io::Error::last_os_error();
            // SAFETY: we own `job` and will not use it again.
            unsafe { windows_sys::Win32::Foundation::CloseHandle(job) };
            return Err(err);
        }

        // SAFETY: both handles are valid; ownership of the process handle
        // transfers to the job (it is closed immediately after).
        let assigned = unsafe { AssignProcessToJobObject(job, process) };
        // SAFETY: the process handle is no longer needed either way.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(process) };
        if assigned == 0 {
            let err = std::io::Error::last_os_error();
            // SAFETY: we own `job` and will not use it again.
            unsafe { windows_sys::Win32::Foundation::CloseHandle(job) };
            return Err(err);
        }

        Ok(Self { handle: job })
    }

    /// Terminate every process in the job (exit code 1).
    pub fn terminate(&self) -> std::io::Result<()> {
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;
        // SAFETY: `self.handle` is a valid job handle owned by `self`.
        if unsafe { TerminateJobObject(self.handle, 1) } == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

#[cfg(windows)]
impl Drop for JobHandle {
    fn drop(&mut self) {
        // Only close — never terminate. KILL_ON_JOB_CLOSE makes the kernel
        // terminate the job's processes when this last handle goes away,
        // which is exactly the drop-time cleanup we want.
        // SAFETY: we own `self.handle` and will not use it again.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.handle) };
    }
}

/// Fallback tree kill for Windows: snapshot the process table, resolve the
/// tree rooted at `root_pid`, and terminate each process leaf-to-root.
///
/// Processes that exited between snapshot and termination (or that cannot be
/// opened) are skipped — this is best-effort by design; the Job Object is
/// the primary mechanism and this only runs when job assignment failed.
///
/// Known trade-off — pid reuse window: between the snapshot and the
/// terminate loop, a tree member can exit and Windows can reuse its pid for
/// an unrelated process, which we would then terminate. Accepted because
/// this is only the degraded path (job assignment failed) and the window is
/// a few milliseconds wide.
#[cfg(windows)]
pub fn kill_process_tree(root_pid: u32) {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};

    // SAFETY: plain Win32 call; the returned snapshot handle is checked.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        tracing::warn!(
            root_pid,
            "kill_process_tree: CreateToolhelp32Snapshot failed: {}",
            std::io::Error::last_os_error()
        );
        return;
    }

    let mut procs: Vec<(u32, u32)> = Vec::new();
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    // SAFETY: `snapshot` is a valid snapshot handle; `entry` is initialized
    // with the documented dwSize the API requires.
    if unsafe { Process32FirstW(snapshot, &mut entry) } != 0 {
        loop {
            procs.push((entry.th32ProcessID, entry.th32ParentProcessID));
            // SAFETY: same as above for subsequent entries.
            if unsafe { Process32NextW(snapshot, &mut entry) } == 0 {
                break;
            }
        }
    }
    // SAFETY: we own the snapshot handle and will not use it again.
    unsafe { CloseHandle(snapshot) };

    // `tree` is parents-before-children; terminate in reverse (leaves
    // first) so a dying parent cannot hand the pid to a new process before
    // we reach its children.
    let tree = collect_descendants(&procs, root_pid);
    for &pid in tree.iter().rev() {
        if pid == std::process::id() {
            // Never terminate ourselves, even if the snapshot claims we are
            // in the tree (corrupted ppid data).
            continue;
        }
        // SAFETY: plain Win32 call with a numeric pid.
        let process = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
        if process.is_null() {
            // Already exited, or access denied — nothing to do for this pid.
            continue;
        }
        // SAFETY: `process` is a valid handle opened above.
        unsafe { TerminateProcess(process, 1) };
        // SAFETY: we own `process` and will not use it again.
        unsafe { CloseHandle(process) };
    }
}

#[cfg(test)]
mod tests {
    use super::collect_descendants;

    /// Given a linear chain of processes, when collecting descendants of the
    /// root, then every pid in the chain appears exactly once, root first.
    #[test]
    fn should_collect_the_whole_chain_when_descendants_form_a_line() {
        // 1 -> 2 -> 3 -> 4
        let procs = [(2, 1), (3, 2), (4, 3)];
        assert_eq!(collect_descendants(&procs, 1), vec![1, 2, 3, 4]);
    }

    /// Given a forked tree, when collecting descendants of the root, then
    /// both branches are covered and parents precede their children.
    #[test]
    fn should_collect_both_branches_when_descendants_fork() {
        //      1
        //     / \
        //    2   3
        //   / \   \
        //  4   5   6
        let procs = [(2, 1), (3, 1), (4, 2), (5, 2), (6, 3)];
        let tree = collect_descendants(&procs, 1);
        assert_eq!(tree.len(), 6, "all six pids must be collected: {tree:?}");
        assert_eq!(tree[0], 1, "the root comes first");
        let pos = |pid: u32| tree.iter().position(|&p| p == pid).expect("pid in tree");
        assert!(
            pos(2) < pos(4) && pos(2) < pos(5),
            "2 precedes its children"
        );
        assert!(pos(3) < pos(6), "3 precedes its children");
    }

    /// Given a snapshot where a parent-child pair references each other (a
    /// corrupted ppid cycle), when collecting descendants, then the walk
    /// terminates and each pid appears exactly once.
    #[test]
    fn should_terminate_when_the_snapshot_contains_a_cycle() {
        // 1 -> 2 -> 3 -> 2 (cycle back)
        let procs = [(2, 1), (3, 2), (2, 3)];
        let tree = collect_descendants(&procs, 1);
        assert_eq!(tree, vec![1, 2, 3], "each pid exactly once, no hang");
    }

    /// Given an unrelated subtree, when collecting descendants of the root,
    /// then the unrelated subtree is excluded.
    #[test]
    fn should_exclude_unrelated_subtrees_when_collecting() {
        // 1 -> 2, and 9 -> 10 unrelated
        let procs = [(2, 1), (10, 9)];
        assert_eq!(collect_descendants(&procs, 1), vec![1, 2]);
    }

    /// Given a root with no children in the snapshot, when collecting
    /// descendants, then the result is just the root.
    #[test]
    fn should_return_only_the_root_when_it_has_no_children() {
        let procs = [(7, 4)];
        assert_eq!(collect_descendants(&procs, 7), vec![7]);
        assert_eq!(collect_descendants(&[], 7), vec![7]);
    }
}
