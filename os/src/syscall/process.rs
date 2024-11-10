//! Process management syscalls
//!
use alloc::sync::Arc;

use crate::{
    config::MAX_SYSCALL_NUM,
    fs::{open_file, OpenFlags},
    mm::{translated_byte_buffer, translated_refmut, translated_str, MapPermission, VirtAddr},
    task::{
        add_task, append_map_area, current_task, current_user_token, exit_current_and_run_next,
        get_task_info, remove_map_area, suspend_current_and_run_next, TaskStatus,
    },
    timer::get_time_us,
};
use core::mem::size_of;

#[repr(C)]
#[derive(Debug)]
pub struct TimeVal {
    pub sec: usize,
    pub usec: usize,
}

/// Task information
#[allow(dead_code)]
pub struct TaskInfo {
    /// Task status in it's life cycle
    status: TaskStatus,
    /// The numbers of syscall called by task
    syscall_times: [u32; MAX_SYSCALL_NUM],
    /// Total running time of task
    time: usize,
}

impl TaskInfo {
    pub fn new(status: TaskStatus, syscall_times: [u32; MAX_SYSCALL_NUM], time: usize) -> Self {
        Self {
            status,
            syscall_times,
            time,
        }
    }
}

/// task exits and submit an exit code
pub fn sys_exit(exit_code: i32) -> ! {
    trace!("kernel:pid[{}] sys_exit", current_task().unwrap().pid.0);
    exit_current_and_run_next(exit_code);
    panic!("Unreachable in sys_exit!");
}

pub fn sys_yield() -> isize {
    //trace!("kernel: sys_yield");
    suspend_current_and_run_next();
    0
}

pub fn sys_getpid() -> isize {
    trace!("kernel: sys_getpid pid:{}", current_task().unwrap().pid.0);
    current_task().unwrap().pid.0 as isize
}

pub fn sys_fork() -> isize {
    trace!("kernel:pid[{}] sys_fork", current_task().unwrap().pid.0);
    let current_task = current_task().unwrap();
    let new_task = current_task.fork();
    let new_pid = new_task.pid.0;
    // modify trap context of new_task, because it returns immediately after switching
    let trap_cx = new_task.inner_exclusive_access().get_trap_cx();
    // we do not have to move to next instruction since we have done it before
    // for child process, fork returns 0
    trap_cx.x[10] = 0;
    // add new task to scheduler
    add_task(new_task);
    new_pid as isize
}

pub fn sys_exec(path: *const u8) -> isize {
    trace!("kernel:pid[{}] sys_exec", current_task().unwrap().pid.0);
    let token = current_user_token();
    let path = translated_str(token, path);
    if let Some(app_inode) = open_file(path.as_str(), OpenFlags::RDONLY) {
        let all_data = app_inode.read_all();
        let task = current_task().unwrap();
        task.exec(all_data.as_slice());
        0
    } else {
        -1
    }
}

/// If there is not a child process whose pid is same as given, return -1.
/// Else if there is a child process but it is still running, return -2.
pub fn sys_waitpid(pid: isize, exit_code_ptr: *mut i32) -> isize {
    trace!(
        "kernel::pid[{}] sys_waitpid [{}]",
        current_task().unwrap().pid.0,
        pid
    );
    let task = current_task().unwrap();
    // find a child process

    // ---- access current PCB exclusively
    let mut inner = task.inner_exclusive_access();
    if !inner
        .children
        .iter()
        .any(|p| pid == -1 || pid as usize == p.getpid())
    {
        return -1;
        // ---- release current PCB
    }
    let pair = inner.children.iter().enumerate().find(|(_, p)| {
        // ++++ temporarily access child PCB exclusively
        p.inner_exclusive_access().is_zombie() && (pid == -1 || pid as usize == p.getpid())
        // ++++ release child PCB
    });
    if let Some((idx, _)) = pair {
        let child = inner.children.remove(idx);
        // confirm that child will be deallocated after being removed from children list
        assert_eq!(Arc::strong_count(&child), 1);
        let found_pid = child.getpid();
        // ++++ temporarily access child PCB exclusively
        let exit_code = child.inner_exclusive_access().exit_code;
        // ++++ release child PCB
        *translated_refmut(inner.memory_set.token(), exit_code_ptr) = exit_code;
        found_pid as isize
    } else {
        -2
    }
    // ---- release current PCB automatically
}

/// YOUR JOB: get time with second and microsecond
/// HINT: You might reimplement it with virtual memory management.
/// HINT: What if [`TimeVal`] is splitted by two pages ?
pub fn sys_get_time(ts: *mut TimeVal, _tz: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_get_time NOT IMPLEMENTED",
        current_task().unwrap().pid.0
    );
    let us = get_time_us();
    let timeval = TimeVal {
        sec: us / 1_000_000,
        usec: us % 1_000_000,
    };
    let ptr = &timeval as *const _ as *const u8;
    let len = size_of::<TimeVal>();
    let timeval = unsafe { core::slice::from_raw_parts(ptr, len) };
    let bufs = translated_byte_buffer(current_user_token(), ts as *const u8, len);
    let mut start: usize = 0;
    for buf in bufs {
        let buf_len = buf.len();
        let src = &timeval[start..len.min(start + buf_len)];
        buf.copy_from_slice(src);
        start += buf_len;
    }
    0
}

/// YOUR JOB: Finish sys_task_info to pass testcases
/// HINT: You might reimplement it with virtual memory management.
/// HINT: What if [`TaskInfo`] is splitted by two pages ?
pub fn sys_task_info(ti: *mut TaskInfo) -> isize {
    trace!(
        "kernel:pid[{}] sys_task_info",
        current_task().unwrap().pid.0
    );

    let taskinfo = get_task_info();
    let ptr = &taskinfo as *const _ as *const u8;
    let len = size_of::<TaskInfo>();
    let taskinfo = unsafe { core::slice::from_raw_parts(ptr, len) };
    let bufs = translated_byte_buffer(current_user_token(), ti as *const u8, len);
    let mut start: usize = 0;
    for buf in bufs {
        let buf_len = buf.len();
        let src = &taskinfo[start..len.min(start + buf_len)];
        buf.copy_from_slice(src);
        start += buf_len;
    }
    0
}

// YOUR JOB: Implement mmap.
pub fn sys_mmap(start: usize, len: usize, port: usize) -> isize {
    trace!("kernel:pid[{}] sys_mmap", current_task().unwrap().pid.0);
    if (port & !0x7 != 0) || (port & 0x7 == 0) {
        return -1;
    }
    let port = port << 1;
    let permission = MapPermission::from_bits(port as u8).unwrap() | MapPermission::U;
    let start_va: VirtAddr = start.into();
    if !start_va.aligned() {
        return -1;
    }
    if len == 0 {
        return 0;
    }
    match append_map_area(start_va, (start + len).into(), permission) {
        Ok(_) => 0,
        _ => -1,
    }
}

// YOUR JOB: Implement munmap.
pub fn sys_munmap(start: usize, len: usize) -> isize {
    trace!("kernel:pid[{}] sys_munmap", current_task().unwrap().pid.0);
    // 找到完全对应的Maparea并删除，返回0，否则返回-1
    // TODO: replace it with `remove_area_with_start_vpn`
    match remove_map_area(start.into(), (start + len).into()) {
        Ok(_) => 0,
        _ => -1,
    }
}

/// change data segment size
pub fn sys_sbrk(size: i32) -> isize {
    trace!("kernel:pid[{}] sys_sbrk", current_task().unwrap().pid.0);
    if let Some(old_brk) = current_task().unwrap().change_program_brk(size) {
        old_brk as isize
    } else {
        -1
    }
}

/// YOUR JOB: Implement spawn.
/// HINT: fork + exec =/= spawn
pub fn sys_spawn(path: *const u8) -> isize {
    trace!("kernel:pid[{}] sys_spawnd", current_task().unwrap().pid.0);
    let token = current_user_token();
    let path = translated_str(token, path);

    let Some(current) = current_task() else {
        return -1;
    };
    let new_task = current.fork();
    let all_data = {
        let Some(app_inode) = open_file(path.as_str(), OpenFlags::RDONLY) else {
            return -1;
        };
        app_inode.read_all()
    };
    let pid = new_task.getpid();
    new_task.exec(all_data.as_slice());
    add_task(new_task);
    pid as isize
}

// YOUR JOB: Set task priority.
pub fn sys_set_priority(prio: isize) -> isize {
    trace!(
        "kernel:pid[{}] sys_set_priorityd",
        current_task().unwrap().pid.0
    );
    if prio >= 2 {
        let current = current_task().unwrap();
        let mut inner = current.inner_exclusive_access();
        inner.priority = prio as usize;
        prio
    } else {
        -1
    }
}
