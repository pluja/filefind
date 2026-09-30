//! Background work must never compete with what the user is doing.

/// Moves the calling thread to idle CPU scheduling and idle I/O priority: it only runs
/// when nothing else wants the CPU or disk. Threads it creates inherit this.
/// Only issues system calls, so it is safe between fork and exec.
pub fn lower_current_thread() {
    const IOPRIO_WHO_PROCESS: libc::c_long = 1;
    const IOPRIO_CLASS_IDLE: libc::c_long = 3;
    const IOPRIO_CLASS_SHIFT: libc::c_long = 13;
    // SAFETY: plain system calls on the current thread (tid 0 / pid 0 = caller).
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, 19);
        let param = libc::sched_param { sched_priority: 0 };
        libc::sched_setscheduler(0, libc::SCHED_IDLE, &param);
        libc::syscall(libc::SYS_ioprio_set, IOPRIO_WHO_PROCESS, 0, IOPRIO_CLASS_IDLE << IOPRIO_CLASS_SHIFT);
    }
}
