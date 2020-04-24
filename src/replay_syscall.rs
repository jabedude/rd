#[cfg(feature = "verify_syscall_numbers")]
include!(concat!(
    env!("OUT_DIR"),
    "/check_syscall_numbers_generated.rs"
));

use crate::address_space::kernel_mapping::KernelMapping;
use crate::auto_remote_syscalls::AutoRemoteSyscalls;
use crate::kernel_abi::{is_write_syscall, SupportedArch};
use crate::kernel_metadata::syscall_name;
use crate::log::LogLevel::LogDebug;
use crate::session::replay_session::ReplaySession;
use crate::task::replay_task::ReplayTask;
use crate::task::task_inner::ResumeRequest;
use crate::task::task_inner::TicksRequest;
use crate::task::task_inner::WaitRequest;
use crate::task::Task;
use crate::trace::trace_frame::FrameTime;
use crate::trace::trace_stream;
use crate::trace::trace_task_event::{TraceTaskEvent, TraceTaskEventType};
use crate::wait_status::WaitStatus;
use libc::pid_t;
use nix::sys::mman::{MapFlags, ProtFlags};
use std::cmp::min;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStringExt;

/// Proceeds until the next system call, which is being executed.
///
/// DIFF NOTE: Params maybe_expect_syscallno2 and maybe_new_tid and treatment slightly different.
fn __ptrace_cont(
    t: &mut ReplayTask,
    resume_how: ResumeRequest,
    syscall_arch: SupportedArch,
    expect_syscallno: i32,
    maybe_expect_syscallno2: Option<i32>,
    maybe_new_tid: Option<pid_t>,
) {
    maybe_expect_syscallno2.map(|n| debug_assert!(n >= 0));
    maybe_new_tid.map(|n| assert!(n > 0));
    let new_tid = maybe_new_tid.unwrap_or(-1);
    let expect_syscallno2 = maybe_expect_syscallno2.unwrap_or(-1);
    t.resume_execution(
        resume_how,
        WaitRequest::ResumeNonblocking,
        TicksRequest::ResumeNoTicks,
        None,
    );
    loop {
        if t.wait_unexpected_exit() {
            break;
        }
        let mut raw_status: i32 = 0;
        // Do our own waitpid instead of calling Task::wait() so we can detect and
        // handle tid changes due to off-main-thread execve.
        // When we're expecting a tid change, we can't pass a tid here because we
        // don't know which tid to wait for.
        // Passing the original tid seems to cause a hang in some kernels
        // (e.g. 4.10.0-19-generic) if the tid change races with our waitpid
        let ret = unsafe { libc::waitpid(new_tid, &mut raw_status, libc::__WALL) };
        ed_assert!(t, ret >= 0);
        if ret == new_tid {
            // Check that we only do this once
            ed_assert!(t, t.tid != new_tid);
            // Update the serial as if this task was really created by cloning the old task.
            t.set_real_tid_and_update_serial(new_tid);
        }
        ed_assert!(t, ret == t.tid);
        t.did_waitpid(WaitStatus::new(raw_status));

        // DIFF NOTE: @TODO The `if` statement logic may create a slight divergence from rr.
        // May need to think about this more deeply and make sure this will work like rr.
        if t.status().stop_sig().is_some()
            && ReplaySession::is_ignored_signal(t.status().stop_sig().unwrap())
        {
            t.resume_execution(
                resume_how,
                WaitRequest::ResumeNonblocking,
                TicksRequest::ResumeNoTicks,
                None,
            );
        } else {
            break;
        }
    }

    ed_assert!(
        t,
        t.stop_sig().is_none(),
        "Expected no pending signal, but got {}",
        t.stop_sig().unwrap()
    );

    // check if we are synchronized with the trace -- should never fail
    let current_syscall = t.regs_ref().original_syscallno() as i32;
    // DIFF NOTE: Minor differences arising out of maybe_dump_written_string() behavior.
    ed_assert!(
        t,
        current_syscall == expect_syscallno || current_syscall == expect_syscallno2,
        "Should be at {}, but instead at {} ({:?})",
        syscall_name(expect_syscallno, syscall_arch),
        syscall_name(current_syscall, syscall_arch),
        maybe_dump_written_string(t)
    );
}

/// DIFF NOTE: In rd we're returning a `None` if this was not a write syscall
fn maybe_dump_written_string(t: &mut ReplayTask) -> Option<OsString> {
    if !is_write_syscall(t.regs_ref().original_syscallno() as i32, t.arch()) {
        return None;
    }
    let len = min(1000, t.regs_ref().arg3());
    let mut buf = Vec::<u8>::with_capacity(len);
    buf.resize(len, 0u8);
    // DIFF NOTE: Here we're actually expecting there to be no Err(_), hence the unwrap()
    let nread = t
        .read_bytes_fallible(t.regs_ref().arg2().into(), &mut buf)
        .unwrap();
    buf.truncate(nread);
    Some(OsString::from_vec(buf))
}

fn init_scratch_memory(t: &mut ReplayTask, km: &KernelMapping, data: &trace_stream::MappedData) {
    ed_assert!(t, data.source == trace_stream::MappedDataSource::SourceZero);

    t.scratch_ptr = km.start();
    t.scratch_size = km.size();
    let sz = t.scratch_size;
    let scratch_ptr = t.scratch_ptr;
    // Make the scratch buffer read/write during replay so that
    // preload's sys_read can use it to buffer cloned data.
    ed_assert!(
        t,
        km.prot()
            .contains(ProtFlags::PROT_READ | ProtFlags::PROT_WRITE)
    );
    ed_assert!(
        t,
        km.flags()
            .contains(MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS)
    );

    {
        {
            let mut remote = AutoRemoteSyscalls::new(t);
            remote.infallible_mmap_syscall(
                Some(scratch_ptr),
                sz,
                km.prot(),
                km.flags() | MapFlags::MAP_FIXED,
                -1,
                0,
            );
        }
        t.vm_mut().map(
            t,
            t.scratch_ptr,
            sz,
            km.prot(),
            km.flags(),
            0,
            OsStr::new(""),
            KernelMapping::NO_DEVICE,
            KernelMapping::NO_INODE,
            None,
            Some(&km),
            None,
            None,
            None,
        );
    }
    t.setup_preload_thread_locals();
}

/// If scratch data was incidentally recorded for the current desched'd
/// but write-only syscall, then do a no-op restore of that saved data
/// to keep the trace in sync.
///
/// Syscalls like `write()` that may-block and are wrapped in the
/// preload library can be desched'd.  When this happens, we save the
/// syscall record's "extra data" as if it were normal scratch space,
/// since it's used that way in effect.  But syscalls like `write()`
/// that don't actually use scratch space don't ever try to restore
/// saved scratch memory during replay.  So, this helper can be used
/// for that class of syscalls.
fn maybe_noop_restore_syscallbuf_scratch(t: &mut ReplayTask) {
    if t.is_in_untraced_syscall() {
        // Untraced syscalls always have t's arch
        log!(
            LogDebug,
            "  noop-restoring scratch for write-only desched'd {}",
            syscall_name(t.regs_ref().original_syscallno() as i32, t.arch())
        );
        t.set_data_from_trace();
    }
}

fn read_task_trace_event(t: &ReplayTask, task_event_type: TraceTaskEventType) -> TraceTaskEvent {
    let mut ttv: Option<TraceTaskEvent>;
    let mut time: FrameTime = 0;
    let shr_ptr = t.session();
    let mut sess = shr_ptr.borrow_mut();
    let tr = sess.as_replay_mut().unwrap().trace_reader_mut();
    loop {
        ttv = tr.read_task_event(Some(&mut time));
        if ttv.is_none() {
            ed_assert!(
                t,
                false,
                "Unable to find TraceTaskEvent; trace is corrupt (did you kill -9 rd?)"
            )
        }

        if time >= t.current_frame_time() || ttv.as_ref().unwrap().event_type() == task_event_type {
            break;
        }
    }
    ed_assert!(t, time == t.current_frame_time());
    ttv.unwrap()
}
