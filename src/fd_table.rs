use crate::address_space::address_space::{AddressSpace, AddressSpaceRef};
use crate::event::Switchable;
use crate::file_monitor::{FileMonitorSharedPtr, LazyOffset, Range};
use crate::kernel_abi::common::preload_interface::preload_globals;
use crate::kernel_abi::common::preload_interface::SYSCALLBUF_FDS_DISABLED_SIZE;
use crate::log::LogLevel::LogDebug;
use crate::remote_ptr::RemotePtr;
use crate::task::record_task::record_task::RecordTask;
use crate::task::replay_task::ReplayTask;
use crate::task::{Task, TaskPtr};
use crate::task_set::TaskSet;
use nix::sys::stat::lstat;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ops::Deref;
use std::rc::{Rc, Weak};

pub type FdTableSharedPtr = Rc<RefCell<FdTable>>;
pub type FdTableSharedWeakPtr = Weak<RefCell<FdTable>>;

#[derive(Clone)]
pub struct FdTable {
    tasks: TaskSet,
    fds: HashMap<i32, FileMonitorSharedPtr>,
    /// Number of elements of `fds` that are >= SYSCALLBUF_FDS_DISABLED_SIZE
    fd_count_beyond_limit: u32,
}

impl Deref for FdTable {
    type Target = TaskSet;

    fn deref(&self) -> &Self::Target {
        &self.tasks
    }
}

/// We DO NOT want Copy or Clone traits
impl FdTable {
    pub fn add_monitor(&mut self, t: &dyn Task, fd: i32, monitor: FileMonitorSharedPtr) {
        // In the future we could support multiple monitors on an fd, but we don't
        // need to yet.
        ed_assert!(
            t,
            !self.is_monitoring(fd),
            "Task {} already monitoring fd {}",
            t.rec_tid,
            fd
        );
        if fd >= SYSCALLBUF_FDS_DISABLED_SIZE && !self.fds.contains_key(&fd) {
            self.fd_count_beyond_limit += 1;
        }

        self.fds.insert(fd, monitor);
        self.update_syscallbuf_fds_disabled(fd);
    }
    pub fn emulate_ioctl(&self, fd: i32, t: &RecordTask, result: &mut u64) -> bool {
        match self.fds.get(&fd) {
            Some(f) => f.borrow_mut().emulate_ioctl(t, result),
            None => false,
        }
    }
    pub fn emulate_fcntl(&self, fd: i32, t: &RecordTask, result: &mut u64) -> bool {
        match self.fds.get(&fd) {
            Some(f) => f.borrow_mut().emulate_fcntl(t, result),
            None => false,
        }
    }
    pub fn emulate_read(
        &self,
        fd: i32,
        t: &RecordTask,
        ranges: &Vec<Range>,
        offset: &LazyOffset,
        result: &mut u64,
    ) -> bool {
        match self.fds.get(&fd) {
            Some(f) => f.borrow().emulate_read(t, ranges, offset, result),
            None => false,
        }
    }
    pub fn filter_getdents(&self, fd: i32, t: &RecordTask) {
        match self.fds.get(&fd) {
            Some(f) => f.borrow_mut().filter_getdents(t),
            None => (),
        }
    }
    pub fn is_rd_fd(&self, fd: i32) -> bool {
        match self.fds.get(&fd) {
            Some(f) => f.borrow().is_rd_fd(),
            None => false,
        }
    }
    pub fn will_write(&self, t: &dyn Task, fd: i32) -> Switchable {
        match self.fds.get(&fd) {
            Some(f) => f.borrow().will_write(t),
            None => Switchable::AllowSwitch,
        }
    }
    /// @TODO Do we want offset to be a move?
    pub fn did_write(&self, fd: i32, ranges: Vec<Range>, offset: &mut LazyOffset) {
        match self.fds.get(&fd) {
            Some(f) => f.borrow_mut().did_write(&ranges, offset),
            None => (),
        }
    }
    pub fn did_dup(&mut self, from: i32, to: i32) {
        if self.fds.contains_key(&from) {
            if to >= SYSCALLBUF_FDS_DISABLED_SIZE && !self.fds.contains_key(&to) {
                self.fd_count_beyond_limit += 1;
            }
            self.fds.insert(to, self.fds[&from].clone());
        } else {
            if to >= SYSCALLBUF_FDS_DISABLED_SIZE && self.fds.contains_key(&to) {
                self.fd_count_beyond_limit -= 1;
            }
            self.fds.remove(&to);
        }
        self.update_syscallbuf_fds_disabled(to);
    }
    pub fn did_close(&mut self, fd: i32) {
        log!(LogDebug, "Close fd {}", fd);
        if fd >= SYSCALLBUF_FDS_DISABLED_SIZE && self.fds.contains_key(&fd) {
            self.fd_count_beyond_limit -= 1;
        }
        self.fds.remove(&fd);
        self.update_syscallbuf_fds_disabled(fd);
    }

    /// Method is called clone() in rr
    pub fn clone_into_task(&self, t: &mut dyn Task) -> FdTableSharedPtr {
        let mut file_mon = FdTable {
            tasks: TaskSet::new(),
            fds: self.fds.clone(),
            fd_count_beyond_limit: self.fd_count_beyond_limit,
        };

        file_mon.tasks.insert(TaskPtr(t.weak_self_ptr()));
        Rc::new(RefCell::new(file_mon))
    }

    pub fn create(&self, t: &dyn Task) -> FdTableSharedPtr {
        let mut file_mon = FdTable {
            tasks: TaskSet::new(),
            fds: Default::default(),
            fd_count_beyond_limit: 0,
        };

        file_mon.tasks.insert(TaskPtr(t.weak_self_ptr()));
        Rc::new(RefCell::new(file_mon))
    }

    pub fn is_monitoring(&self, fd: i32) -> bool {
        self.fds.contains_key(&fd)
    }
    pub fn count_beyond_limit(&self) -> u32 {
        self.fd_count_beyond_limit
    }

    pub fn get_monitor(self, fd: i32) -> Option<FileMonitorSharedPtr> {
        self.fds.get(&fd).map(|f| f.clone())
    }

    /// Regenerate syscallbuf_fds_disabled in task `t`.
    /// Called during initialization of the preload library.
    pub fn init_syscallbuf_fds_disabled(&self, t: &mut dyn Task) {
        if !t.session().borrow().is_recording() {
            return;
        }

        let rt = t.as_record_task_mut().unwrap();

        ed_assert!(&rt, self.has_task(rt.weak_self_ptr()));

        let preload_globals = match rt.preload_globals {
            None => return,
            Some(pr) => pr,
        };

        let mut disabled: [u8; SYSCALLBUF_FDS_DISABLED_SIZE as usize] =
            [0u8; SYSCALLBUF_FDS_DISABLED_SIZE as usize];

        // It's possible that some tasks in this address space have a different
        // FdTable. We need to disable syscallbuf for an fd if any tasks for this
        // address space are monitoring the fd.
        for vm_t in rt.vm().task_set_iter() {
            for &fd in vm_t.borrow().fd_table().borrow().fds.keys() {
                debug_assert!(fd >= 0);
                let mut adjusted_fd = fd;
                if fd >= SYSCALLBUF_FDS_DISABLED_SIZE {
                    adjusted_fd = SYSCALLBUF_FDS_DISABLED_SIZE - 1;
                }
                disabled[adjusted_fd as usize] = 1;
            }
        }

        let addr: RemotePtr<u8> =
            RemotePtr::cast(preload_globals) + offset_of!(preload_globals, syscallbuf_fds_disabled);
        rt.write_bytes(addr, &disabled);
        rt.record_local(addr, &disabled);
    }

    /// Get list of fds that have been closed after `t` has done an execve.
    /// Rather than tracking CLOEXEC flags (which would be complicated), we just
    /// scan /proc/<pid>/fd during recording and note any monitored fds that have
    /// been closed.
    /// This also updates our table to match reality.
    pub fn fds_to_close_after_exec(&mut self, t: &RecordTask) -> Vec<i32> {
        ed_assert!(t, self.has_task(t.weak_self_ptr()));

        let mut fds_to_close: Vec<i32> = Vec::new();
        for &fd in self.fds.keys() {
            if !is_fd_open(t, fd) {
                fds_to_close.push(fd);
            }
        }
        for &fd in &fds_to_close {
            self.did_close(fd);
        }

        fds_to_close
    }

    /// Close fds in list after an exec.
    pub fn close_after_exec(&mut self, t: &ReplayTask, fds_to_close: &[i32]) {
        ed_assert!(t, self.has_task(t.weak_self_ptr()));

        for &fd in fds_to_close {
            self.did_close(fd)
        }
    }

    fn new() -> FdTable {
        FdTable {
            tasks: Default::default(),
            fds: Default::default(),
            fd_count_beyond_limit: 0,
        }
    }

    fn update_syscallbuf_fds_disabled(&self, mut fd: i32) {
        debug_assert!(fd >= 0);
        debug_assert!(!self.task_hashset().is_empty());

        let mut vms_updated: HashSet<*const AddressSpace> = HashSet::new();
        // It's possible for tasks with different VMs to share this fd table.
        // But tasks with the same VM might have different fd tables...
        for t in self.task_set_iter() {
            if !t.borrow().session().borrow().is_recording() {
                return;
            }

            let mut t_ref_task = t.borrow_mut();
            let rt: &mut RecordTask = t_ref_task.as_record_task_mut().unwrap();

            let vm_addr = rt.vm_as_ptr();
            if !vms_updated.contains(&vm_addr) {
                continue;
            }
            vms_updated.insert(vm_addr);

            if rt.preload_globals.is_some() {
                if fd >= SYSCALLBUF_FDS_DISABLED_SIZE {
                    fd = SYSCALLBUF_FDS_DISABLED_SIZE - 1;
                }
                let disable: u8 = if is_fd_monitored_in_any_task(rt.vm(), fd) {
                    1
                } else {
                    0
                };

                let addr: RemotePtr<u8> = RemotePtr::cast(rt.preload_globals.unwrap())
                    + offset_of!(preload_globals, syscallbuf_fds_disabled)
                    + fd as usize;
                rt.write_bytes(addr, &disable.to_le_bytes());
                rt.record_local(addr, &disable.to_le_bytes());
            }
        }
    }
}

fn is_fd_open(t: &dyn Task, fd: i32) -> bool {
    let path = format!("/proc/{}/fd/{}", t.tid, fd);
    lstat(path.as_str()).is_ok()
}

fn is_fd_monitored_in_any_task(vm: AddressSpaceRef, fd: i32) -> bool {
    for t in vm.task_set_iter() {
        let table = t.borrow().fd_table();
        if table.borrow().is_monitoring(fd)
            || (fd >= SYSCALLBUF_FDS_DISABLED_SIZE - 1 && table.borrow().count_beyond_limit() > 0)
        {
            return true;
        }
    }
    false
}
