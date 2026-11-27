// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：平台能力 —— 内存/CPU/进程、磁盘用量、端口探活、跑外部命令（带超时）、
//       只读 sqlite 查询。crate 级共享（hostwork 与 w5a 都从这里取，不再各留一份）。
// 不做：不做业务判断。所有"取不到"都返回 None，由调用方决定回 null 还是空表。
//
// 三个平台各一套实现，公共门面签名一致：
//   macOS  —— libproc + mach（与 psutil 的 macOS 后端同源，数字要对得上）
//   Linux  —— /proc + sysconf（psutil 的 Linux 后端也是读这里）
//   Windows— kernel32 / ntdll 的 extern 声明（不引第三方 crate）
// 平台分支用 `cfg(any(target_os = ..., feature = "sim_*"))`；sim_* 只用于在 macOS 上
// `cargo check` 另一套实现的类型，**不能 build**（Windows 的 extern 符号在本机没有）。
#![allow(dead_code)]

use std::path::Path;
use std::time::{Duration, Instant};

// ------------------------------------------------------------------ mach / libproc

#[cfg(target_os = "macos")]
mod mac {
    use std::os::raw::{c_char, c_int, c_uint, c_void};

    pub type MachPort = c_uint;
    pub type KernReturn = c_int;
    pub type NaturalT = c_uint;
    pub type IntegerT = c_int;

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    pub struct VmStatistics64 {
        pub free_count: NaturalT,
        pub active_count: NaturalT,
        pub inactive_count: NaturalT,
        pub wire_count: NaturalT,
        pub zero_fill_count: u64,
        pub reactivations: u64,
        pub pageins: u64,
        pub pageouts: u64,
        pub faults: u64,
        pub cow_faults: u64,
        pub lookups: u64,
        pub hits: u64,
        pub purges: u64,
        pub purgeable_count: NaturalT,
        pub speculative_count: NaturalT,
        pub decompressions: u64,
        pub compressions: u64,
        pub swapins: u64,
        pub swapouts: u64,
        pub compressor_page_count: NaturalT,
        pub throttled_count: NaturalT,
        pub external_page_count: NaturalT,
        pub internal_page_count: NaturalT,
        pub total_uncompressed_pages_in_compressor: u64,
    }

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    pub struct HostCpuLoadInfo {
        pub cpu_ticks: [NaturalT; 4], // user, system, idle, nice
    }

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    pub struct ProcTaskInfo {
        pub pti_virtual_size: u64,
        pub pti_resident_size: u64,
        pub pti_total_user: u64,
        pub pti_total_system: u64,
        pub pti_threads_user: u64,
        pub pti_threads_system: u64,
        pub pti_policy: i32,
        pub pti_faults: i32,
        pub pti_pageins: i32,
        pub pti_cow_faults: i32,
        pub pti_messages_sent: i32,
        pub pti_messages_received: i32,
        pub pti_syscalls_mach: i32,
        pub pti_syscalls_unix: i32,
        pub pti_csw: i32,
        pub pti_threadnum: i32,
        pub pti_numrunning: i32,
        pub pti_priority: i32,
    }

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    pub struct ProcBsdInfo {
        pub pbi_flags: u32,
        pub pbi_status: u32,
        pub pbi_xstatus: u32,
        pub pbi_pid: u32,
        pub pbi_ppid: u32,
        pub pbi_uid: u32,
        pub pbi_gid: u32,
        pub pbi_ruid: u32,
        pub pbi_rgid: u32,
        pub pbi_svuid: u32,
        pub pbi_svgid: u32,
        pub rfu_1: u32,
        pub pbi_comm: [c_char; 16],
        pub pbi_name: [c_char; 32],
        pub pbi_nfiles: u32,
        pub pbi_pgid: u32,
        pub pbi_pjobc: u32,
        pub e_tdev: u32,
        pub e_tpgid: u32,
        pub pbi_nice: i32,
        pub pbi_start_tvsec: u64,
        pub pbi_start_tvusec: u64,
    }

    extern "C" {
        pub fn mach_host_self() -> MachPort;
        pub fn mach_task_self() -> MachPort;
        pub fn host_statistics64(
            host: MachPort,
            flavor: c_int,
            info: *mut c_void,
            count: *mut c_uint,
        ) -> KernReturn;
        pub fn host_statistics(host: MachPort, flavor: c_int, info: *mut c_void, count: *mut c_uint)
            -> KernReturn;
        pub fn mach_port_deallocate(task: MachPort, name: MachPort) -> KernReturn;

        pub fn proc_listallpids(buffer: *mut c_void, buffersize: c_int) -> c_int;
        pub fn proc_pidinfo(
            pid: c_int,
            flavor: c_int,
            arg: u64,
            buffer: *mut c_void,
            buffersize: c_int,
        ) -> c_int;
        pub fn proc_pidpath(pid: c_int, buffer: *mut c_void, buffersize: u32) -> c_int;
        pub fn proc_pid_rusage(pid: c_int, flavor: c_int, buffer: *mut c_void) -> c_int;
        pub fn proc_pidfdinfo(
            pid: c_int,
            fd: c_int,
            flavor: c_int,
            buffer: *mut c_void,
            buffersize: c_int,
        ) -> c_int;
    }

    pub const HOST_VM_INFO64: c_int = 4;
    pub const HOST_CPU_LOAD_INFO: c_int = 3;
    pub const PROC_PIDTASKINFO: c_int = 4;
    pub const PROC_PIDTBSDINFO: c_int = 3;
    pub const RUSAGE_INFO_V2: c_int = 2;
    pub const PROC_PIDLISTFDS: c_int = 1;
    pub const PROC_PIDFDVNODEPATHINFO: c_int = 2;
    pub const PROX_FDTYPE_VNODE: u32 = 1;

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    pub struct RUsageInfoV2 {
        pub ri_uuid: [u8; 16],
        pub ri_user_time: u64,
        pub ri_system_time: u64,
        pub ri_pkg_idle_wkups: u64,
        pub ri_interrupt_wkups: u64,
        pub ri_pageins: u64,
        pub ri_wired_size: u64,
        pub ri_resident_size: u64,
        pub ri_phys_footprint: u64,
        pub ri_proc_start_abstime: u64,
        pub ri_proc_exit_abstime: u64,
        pub ri_child_user_time: u64,
        pub ri_child_system_time: u64,
        pub ri_child_pkg_idle_wkups: u64,
        pub ri_child_interrupt_wkups: u64,
        pub ri_child_pageins: u64,
        pub ri_child_elapsed_abstime: u64,
        pub ri_diskio_bytesread: u64,
        pub ri_diskio_byteswritten: u64,
    }
}

/// 物理页大小（arm64 是 16384，x86 是 4096）。
#[cfg(all(
    target_os = "macos",
    not(feature = "sim_linux"),
    not(feature = "sim_windows")
))]
pub fn page_size() -> u64 {
    unsafe { libc::sysconf(libc::_SC_PAGESIZE) as u64 }
}

/// 在线 CPU 数（`os.cpu_count()` 的等价物）。
#[cfg(all(
    target_os = "macos",
    not(feature = "sim_linux"),
    not(feature = "sim_windows")
))]
pub fn cpu_count() -> i64 {
    let n = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
    if n > 0 {
        n as i64
    } else {
        1
    }
}

/// 开机时刻（epoch 秒，sysctl `kern.boottime`）。原来写在 procs.rs 里，
/// 挪进来是因为三平台的取法完全不同（Linux 读 btime、Windows 用开机毫秒数）。
#[cfg(all(
    target_os = "macos",
    not(feature = "sim_linux"),
    not(feature = "sim_windows")
))]
pub fn boot_time_secs() -> Option<f64> {
    #[repr(C)]
    struct TimeVal {
        tv_sec: i64,
        tv_usec: i32,
    }
    let mut tv = TimeVal { tv_sec: 0, tv_usec: 0 };
    let mut len = std::mem::size_of::<TimeVal>() as libc::size_t;
    let name = std::ffi::CString::new("kern.boottime").ok()?;
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            &mut tv as *mut _ as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc == 0 && tv.tv_sec > 0 {
        Some(tv.tv_sec as f64 + tv.tv_usec as f64 / 1_000_000.0)
    } else {
        None
    }
}

/// 物理内存总量（sysctl hw.memsize）。
#[cfg(all(
    target_os = "macos",
    not(feature = "sim_linux"),
    not(feature = "sim_windows")
))]
pub fn mem_total() -> Option<u64> {
    let mut size: u64 = 0;
    let mut len = std::mem::size_of::<u64>() as libc::size_t;
    let name = std::ffi::CString::new("hw.memsize").ok()?;
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            &mut size as *mut u64 as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc == 0 {
        Some(size)
    } else {
        None
    }
}

/// (free, active, inactive, wired, speculative) 页数。
#[cfg(all(
    target_os = "macos",
    not(feature = "sim_linux"),
    not(feature = "sim_windows")
))]
pub fn vm_counts() -> Option<(u64, u64, u64, u64, u64)> {
    unsafe {
        let host = mac::mach_host_self();
        let mut st = mac::VmStatistics64::default();
        let mut count = (std::mem::size_of::<mac::VmStatistics64>() / std::mem::size_of::<u32>())
            as u32;
        let kr = mac::host_statistics64(
            host,
            mac::HOST_VM_INFO64,
            &mut st as *mut _ as *mut std::ffi::c_void,
            &mut count,
        );
        mac::mach_port_deallocate(mac::mach_task_self(), host);
        if kr != 0 {
            return None;
        }
        Some((
            st.free_count as u64,
            st.active_count as u64,
            st.inactive_count as u64,
            st.wire_count as u64,
            st.speculative_count as u64,
        ))
    }
}

/// (user, system, idle, nice) 累计 tick。psutil 的 cpu_times 就是这个。
#[cfg(all(
    target_os = "macos",
    not(feature = "sim_linux"),
    not(feature = "sim_windows")
))]
pub fn cpu_ticks() -> Option<(u64, u64, u64, u64)> {
    unsafe {
        let host = mac::mach_host_self();
        let mut info = mac::HostCpuLoadInfo::default();
        let mut count =
            (std::mem::size_of::<mac::HostCpuLoadInfo>() / std::mem::size_of::<u32>()) as u32;
        let kr = mac::host_statistics(
            host,
            mac::HOST_CPU_LOAD_INFO,
            &mut info as *mut _ as *mut std::ffi::c_void,
            &mut count,
        );
        mac::mach_port_deallocate(mac::mach_task_self(), host);
        if kr != 0 {
            return None;
        }
        // psutil 的 scputimes 顺序：user, nice, system, idle
        Some((
            info.cpu_ticks[0] as u64,
            info.cpu_ticks[1] as u64,
            info.cpu_ticks[2] as u64,
            info.cpu_ticks[3] as u64,
        ))
    }
}

/// 全部 pid。
#[cfg(all(
    target_os = "macos",
    not(feature = "sim_linux"),
    not(feature = "sim_windows")
))]
pub fn list_pids() -> Vec<i32> {
    unsafe {
        // proc_listallpids(NULL, 0) 返回当前进程数，留一倍余量
        let n = mac::proc_listallpids(std::ptr::null_mut(), 0);
        if n <= 0 {
            return Vec::new();
        }
        let cap = (n as usize) * 2 + 64;
        let mut buf: Vec<i32> = vec![0; cap];
        let got = mac::proc_listallpids(
            buf.as_mut_ptr() as *mut std::ffi::c_void,
            (cap * std::mem::size_of::<i32>()) as i32,
        );
        if got <= 0 {
            return Vec::new();
        }
        buf.truncate(got as usize);
        buf.retain(|p| *p > 0);
        // psutil.pids() 在 mac 上返回**升序** pid，进程遍历顺序因此也是升序。
        // 候选列表的「第一个 Ollama 子进程 / 第一个主进程」依赖这个顺序，
        // 不排序就会和 Python 挑到不同的进程，ctx / pid 跟着错位。
        buf.sort_unstable();
        buf
    }
}

/// (rss 字节, 累计 user+system 微秒)
#[cfg(all(
    target_os = "macos",
    not(feature = "sim_linux"),
    not(feature = "sim_windows")
))]
pub fn proc_taskinfo(pid: i32) -> Option<(u64, u64)> {
    unsafe {
        let mut ti = mac::ProcTaskInfo::default();
        let sz = std::mem::size_of::<mac::ProcTaskInfo>() as i32;
        let got = mac::proc_pidinfo(
            pid,
            mac::PROC_PIDTASKINFO,
            0,
            &mut ti as *mut _ as *mut std::ffi::c_void,
            sz,
        );
        if got <= 0 {
            return None;
        }
        Some((ti.pti_resident_size, ti.pti_total_user + ti.pti_total_system))
    }
}

/// 进程 argv。
///
/// macOS：`sysctl(CTL_KERN, KERN_PROCARGS2, pid)`。返回的字节区布局是：
///   4 字节 argc（本机字节序）→ exec path（NUL 结尾）→ 若干 NUL 填充 →
///   argc 个 NUL 结尾的参数字符串 → 环境变量。
/// 读别的用户的进程会 EPERM，进程刚好退出会 ESRCH；两种都返回空 vec ——
/// 只是这一项的 argv 没有，name/ppid/rss 照常，不能让整张进程表失败。
#[cfg(all(
    target_os = "macos",
    not(feature = "sim_linux"),
    not(feature = "sim_windows")
))]
pub fn proc_argv(pid: i32) -> Vec<String> {
    let mut mib: [libc::c_int; 3] = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut size: libc::size_t = 0;
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    // 有的内核版本不接受 NULL 查询大小，退一个够大的固定缓冲。
    if rc != 0 || size == 0 {
        size = 256 * 1024;
    }
    for _ in 0..2 {
        let mut buf = vec![0u8; size];
        let mut len = size;
        let rc = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                3,
                buf.as_mut_ptr() as *mut libc::c_void,
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc == 0 {
            if len < 4 {
                return Vec::new();
            }
            buf.truncate(len);
            return parse_procargs2(&buf);
        }
        // ENOMEM 时内核把所需长度写回 len，扩一次再试
        if len > size {
            size = len;
            continue;
        }
        return Vec::new();
    }
    Vec::new()
}

#[cfg(all(
    target_os = "macos",
    not(feature = "sim_linux"),
    not(feature = "sim_windows")
))]
fn parse_procargs2(buf: &[u8]) -> Vec<String> {
    if buf.len() < 4 {
        return Vec::new();
    }
    let argc = i32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if argc <= 0 || argc > 65536 {
        return Vec::new();
    }
    let mut i = 4usize;
    // exec path：到第一个 NUL
    while i < buf.len() && buf[i] != 0 {
        i += 1;
    }
    // exec path 之后可能还有若干 NUL 填充，全部跳过
    while i < buf.len() && buf[i] == 0 {
        i += 1;
    }
    let mut out = Vec::with_capacity(argc as usize);
    for _ in 0..argc {
        if i >= buf.len() {
            break;
        }
        let start = i;
        while i < buf.len() && buf[i] != 0 {
            i += 1;
        }
        out.push(String::from_utf8_lossy(&buf[start..i]).to_string());
        if i < buf.len() {
            i += 1; // 跳过该参数结尾的 NUL
        }
    }
    out
}

/// Linux：`/proc/<pid>/cmdline`，NUL 分隔。
#[cfg(any(target_os = "linux", feature = "sim_linux"))]
pub fn proc_argv(pid: i32) -> Vec<String> {
    parse_cmdline(&match std::fs::read(format!("/proc/{pid}/cmdline")) {
        Ok(b) => b,
        Err(_) => return Vec::new(),
    })
}

/// NUL 分隔的命令行 → argv。单独拎出来是为了能在 macOS 上跑单测。
fn parse_cmdline(raw: &[u8]) -> Vec<String> {
    raw.split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).to_string())
        .collect()
}

/// Windows 的 argv：**本函数不实现**。
///
/// 读 PEB 的 `ProcessLine` 要 NtQueryInformationProcess + ReadProcessMemory + 处理
/// 32/64 位跨位宽，代价大且容易在受保护进程上失败；逐 pid 起一次 PowerShell 又慢
/// 到不可用（几百个进程 = 几百次进程创建）。批量实现放在调用方
/// `hostwork::procs::collect_procs()`：一次 `Get-CimInstance Win32_Process` 拿全表，
/// 再按 pid 查。这里如实返回空，不编一个假的 `[exe]`。
#[cfg(any(windows, feature = "sim_windows"))]
pub fn proc_argv(_pid: i32) -> Vec<String> {
    Vec::new()
}

/// 进程可执行文件路径。
#[cfg(all(
    target_os = "macos",
    not(feature = "sim_linux"),
    not(feature = "sim_windows")
))]
pub fn proc_path(pid: i32) -> Option<String> {
    unsafe {
        let mut buf = vec![0u8; 4096];
        let n = mac::proc_pidpath(pid, buf.as_mut_ptr() as *mut std::ffi::c_void, 4096);
        if n <= 0 {
            return None;
        }
        let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
        Some(String::from_utf8_lossy(&buf[..end]).to_string())
    }
}

/// `_fd_hint(pid)`：这个进程打开的文件路径（截前 limit 个）。
///
/// 采样里唯一的用途是「认出跑脚本的进程属于谁」：`python` 只有可执行文件
/// 路径，`/moshan/`、`local-ai-workspace/pocket` 这些线索全在它打开的文件上。
/// 失败返回空串，不影响采样。
#[cfg(all(
    target_os = "macos",
    not(feature = "sim_linux"),
    not(feature = "sim_windows")
))]
pub fn proc_fd_paths(pid: i32, limit: usize) -> String {
    unsafe {
        let need = mac::proc_pidinfo(pid, mac::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0);
        if need <= 0 {
            return String::new();
        }
        let step = 8usize; // struct proc_fdinfo { int32 proc_fd; uint32 proc_fdtype; }
        let cap = need as usize + step * 8;
        let mut buf = vec![0u8; cap];
        let got = mac::proc_pidinfo(
            pid,
            mac::PROC_PIDLISTFDS,
            0,
            buf.as_mut_ptr() as *mut std::ffi::c_void,
            cap as i32,
        );
        if got <= 0 {
            return String::new();
        }
        let mut out: Vec<String> = Vec::new();
        let mut vnode = vec![0u8; 8192];
        let n = (got as usize / step).min(limit * 3);
        for i in 0..n {
            let off = i * step;
            let fd = i32::from_ne_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]);
            let ftype = u32::from_ne_bytes([buf[off + 4], buf[off + 5], buf[off + 6], buf[off + 7]]);
            if ftype != mac::PROX_FDTYPE_VNODE {
                continue;
            }
            vnode.iter_mut().for_each(|b| *b = 0);
            let r = mac::proc_pidfdinfo(
                pid,
                fd,
                mac::PROC_PIDFDVNODEPATHINFO,
                vnode.as_mut_ptr() as *mut std::ffi::c_void,
                vnode.len() as i32,
            );
            if r <= 0 {
                continue;
            }
            // 整个 vnode 结构前面都是二进制，路径在尾巴上：扫出可打印的 `/…` 段
            let raw = &vnode[..r as usize];
            let mut taken = 0;
            let mut j = 0;
            while j < raw.len() && taken < 2 {
                if raw[j] == b'/' {
                    let mut k = j;
                    while k < raw.len() && (0x20..=0x7e).contains(&raw[k]) {
                        k += 1;
                    }
                    if k - j >= 4 {
                        out.push(String::from_utf8_lossy(&raw[j..k]).to_string());
                        taken += 1;
                    }
                    j = k;
                } else {
                    j += 1;
                }
            }
            if out.len() >= limit {
                break;
            }
        }
        out.join(" ")
    }
}

/// (pid, ppid, start_tvsec, comm)
#[cfg(all(
    target_os = "macos",
    not(feature = "sim_linux"),
    not(feature = "sim_windows")
))]
pub fn proc_bsdinfo(pid: i32) -> Option<(i32, i32, u64, String)> {
    unsafe {
        let mut bi = mac::ProcBsdInfo::default();
        let sz = std::mem::size_of::<mac::ProcBsdInfo>() as i32;
        let got = mac::proc_pidinfo(
            pid,
            mac::PROC_PIDTBSDINFO,
            0,
            &mut bi as *mut _ as *mut std::ffi::c_void,
            sz,
        );
        if got <= 0 {
            return None;
        }
        // psutil 的 name() 也是这个口径：pbi_name 为空时退 pbi_comm。
        let end = bi
            .pbi_name
            .iter()
            .position(|c| *c == 0)
            .unwrap_or(bi.pbi_name.len());
        let mut name: Vec<u8> = bi.pbi_name[..end].iter().map(|c| *c as u8).collect();
        if name.is_empty() {
            let cend = bi.pbi_comm.iter().position(|c| *c == 0).unwrap_or(16);
            name = bi.pbi_comm[..cend].iter().map(|c| *c as u8).collect();
        }
        Some((
            bi.pbi_pid as i32,
            bi.pbi_ppid as i32,
            bi.pbi_start_tvsec,
            String::from_utf8_lossy(&name).to_string(),
        ))
    }
}

/// `proc_pid_rusage(pid, RUSAGE_INFO_V6)` 里的 (磁盘读字节, 磁盘写字节, 能耗纳焦)。
/// Python 侧按 `<53Q` 从偏移 16 解，索引 16/17/40 —— 这里按同一份偏移取。
#[cfg(all(
    target_os = "macos",
    not(feature = "sim_linux"),
    not(feature = "sim_windows")
))]
pub fn proc_rusage(pid: i32) -> Option<(u64, u64, u64)> {
    unsafe {
        let mut buf = vec![0u8; 8192];
        let rc = mac::proc_pid_rusage(pid, 6, buf.as_mut_ptr() as *mut std::ffi::c_void);
        if rc != 0 {
            return None;
        }
        let at = |i: usize| -> Option<u64> {
            let off = 16 + i * 8;
            let b = buf.get(off..off + 8)?;
            Some(u64::from_le_bytes(b.try_into().ok()?))
        };
        Some((at(16)?, at(17)?, at(40)?))
    }
}

// ------------------------------------------------------------------ Linux（/proc）
//
// psutil 在 Linux 上也是读 /proc + sysconf，所以两边同源。
// 解析都做成纯函数（输入一段文本、输出数值），[`tests`] 用固定样例断言 ——
// 这些测试在 macOS 上 `cargo test --features sim_linux` 能真跑。

#[cfg(any(target_os = "linux", feature = "sim_linux"))]
mod linux {
    /// `/proc/meminfo` 里 `Key:  123 kB` 的数值（单位 kB，不做换算）。
    pub fn meminfo_kb(text: &str, key: &str) -> Option<u64> {
        for line in text.lines() {
            let Some(rest) = line.strip_prefix(key) else {
                continue;
            };
            let Some(rest) = rest.strip_prefix(':') else {
                continue;
            };
            return rest.split_whitespace().next()?.parse().ok();
        }
        None
    }

    /// `/proc/stat` 第一行 `cpu  user nice system idle ...`。
    ///
    /// 返回顺序与 macOS 分支**保持一致**：user, system, idle, nice。
    /// （psutil 的 scputimes 是 user/nice/system/idle，本函数刻意不跟它排，
    /// 调用方按索引 2 取 idle —— 两个分支同序才不会一边对一边错。）
    pub fn stat_cpu(text: &str) -> Option<(u64, u64, u64, u64)> {
        let line = text.lines().find(|l| l.starts_with("cpu "))?;
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 5 {
            return None;
        }
        let g = |i: usize| f.get(i)?.parse::<u64>().ok();
        Some((g(1)?, g(3)?, g(4)?, g(2)?))
    }

    /// `/proc/stat` 的 `btime 1700000000`（开机 epoch 秒）。
    pub fn stat_btime(text: &str) -> Option<u64> {
        text.lines()
            .find_map(|l| l.strip_prefix("btime ")?.trim().parse().ok())
    }

    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct ProcStat {
        pub ppid: i32,
        pub utime: u64,
        pub stime: u64,
        pub starttime: u64,
    }

    /// `/proc/<pid>/stat`。第 2 字段 `(comm)` 里可能有空格和括号，
    /// 所以必须从**最后一个** `)` 往后切，按空白 split 会在进程名带空格时错位。
    pub fn proc_stat(text: &str) -> Option<ProcStat> {
        let close = text.rfind(')')?;
        let f: Vec<&str> = text[close + 1..].split_whitespace().collect();
        // f[0]=state(3) f[1]=ppid(4) … f[11]=utime(14) f[12]=stime(15)
        // … f[19]=starttime(22)
        Some(ProcStat {
            ppid: f.get(1)?.parse().ok()?,
            utime: f.get(11)?.parse().ok()?,
            stime: f.get(12)?.parse().ok()?,
            starttime: f.get(19)?.parse().ok()?,
        })
    }

    /// `/proc/<pid>/statm` 第 2 字段：常驻页数。
    pub fn statm_resident(text: &str) -> Option<u64> {
        text.split_whitespace().nth(1)?.parse().ok()
    }

    /// `/proc/<pid>/io` 里 `read_bytes: 123` 这类计数。
    pub fn io_bytes(text: &str, key: &str) -> Option<u64> {
        for line in text.lines() {
            let Some(rest) = line.strip_prefix(key) else {
                continue;
            };
            let Some(rest) = rest.strip_prefix(':') else {
                continue;
            };
            return rest.trim().parse().ok();
        }
        None
    }
}

#[cfg(any(target_os = "linux", feature = "sim_linux"))]
fn clk_tck() -> u64 {
    let v = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if v > 0 {
        v as u64
    } else {
        100 // 内核默认值；sysconf 取不到时才用
    }
}

#[cfg(any(target_os = "linux", feature = "sim_linux"))]
pub fn page_size() -> u64 {
    let v = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if v > 0 {
        v as u64
    } else {
        4096
    }
}

#[cfg(any(target_os = "linux", feature = "sim_linux"))]
pub fn cpu_count() -> i64 {
    let n = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
    if n > 0 {
        n as i64
    } else {
        1
    }
}

/// `/proc/meminfo` 的 `MemTotal`（kB）× 1024。
#[cfg(any(target_os = "linux", feature = "sim_linux"))]
pub fn mem_total() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    linux::meminfo_kb(&text, "MemTotal").map(|kb| kb.saturating_mul(1024))
}

/// (free, active, inactive, wired, speculative) 页数 —— **这是近似，不是 macOS 那五类**。
///
/// Linux 没有 inactive/wired/speculative 的对应物，硬凑出来只会让 `mem()` 的
/// `avail = free + inactive` 偏离 psutil。psutil 在 Linux 上 `available` 用的就是
/// `MemAvailable`，所以这里把 `MemAvailable` 直接折成 free 页、其余四类给 0，
/// 让 `mem()` 原样算出 `avail = MemAvailable`（只差向下取整到页，< 4 KB）。
/// 若老内核没有 MemAvailable，退 `MemFree + Buffers + Cached`（psutil 的兜底同此）。
#[cfg(any(target_os = "linux", feature = "sim_linux"))]
pub fn vm_counts() -> Option<(u64, u64, u64, u64, u64)> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kb = linux::meminfo_kb(&text, "MemAvailable").or_else(|| {
        let f = linux::meminfo_kb(&text, "MemFree")?;
        let b = linux::meminfo_kb(&text, "Buffers").unwrap_or(0);
        let c = linux::meminfo_kb(&text, "Cached").unwrap_or(0);
        Some(f + b + c)
    })?;
    let pages = kb.saturating_mul(1024) / page_size().max(1);
    Some((pages, 0, 0, 0, 0))
}

/// `/proc/stat` 第一行的累计 tick（user, system, idle, nice）。
#[cfg(any(target_os = "linux", feature = "sim_linux"))]
pub fn cpu_ticks() -> Option<(u64, u64, u64, u64)> {
    let text = std::fs::read_to_string("/proc/stat").ok()?;
    linux::stat_cpu(&text)
}

/// 全部 pid：`/proc` 下的纯数字目录名，升序（与 psutil.pids() 同序）。
#[cfg(any(target_os = "linux", feature = "sim_linux"))]
pub fn list_pids() -> Vec<i32> {
    let mut out = Vec::new();
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return out;
    };
    for ent in dir.flatten() {
        if let Ok(pid) = ent.file_name().to_string_lossy().parse::<i32>() {
            if pid > 0 {
                out.push(pid);
            }
        }
    }
    out.sort_unstable();
    out
}

/// (rss 字节, 累计 user+system 微秒)
#[cfg(any(target_os = "linux", feature = "sim_linux"))]
pub fn proc_taskinfo(pid: i32) -> Option<(u64, u64)> {
    let statm = std::fs::read_to_string(format!("/proc/{pid}/statm")).ok()?;
    let resident = linux::statm_resident(&statm)?;
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let info = linux::proc_stat(&stat)?;
    let tck = clk_tck();
    let cpu_us = info.utime.saturating_add(info.stime).saturating_mul(1_000_000) / tck;
    Some((resident.saturating_mul(page_size()), cpu_us))
}

/// `readlink /proc/<pid>/exe`。读不到（无权/已退出）返回 None。
#[cfg(any(target_os = "linux", feature = "sim_linux"))]
pub fn proc_path(pid: i32) -> Option<String> {
    std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .map(|p| p.to_string_lossy().to_string())
}

/// `/proc/<pid>/fd` 各符号链接指向的路径，空格连（与 macOS 版同形状）。
#[cfg(any(target_os = "linux", feature = "sim_linux"))]
pub fn proc_fd_paths(pid: i32, limit: usize) -> String {
    let mut out: Vec<String> = Vec::new();
    let Ok(dir) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
        return String::new();
    };
    for fd in dir.flatten() {
        if out.len() >= limit {
            break;
        }
        if let Ok(link) = std::fs::read_link(fd.path()) {
            let s = link.to_string_lossy().to_string();
            // socket:[123] / anon_inode:[...] 不是路径，跟 macOS 的 vnode 过滤同口径
            if s.starts_with('/') {
                out.push(s);
            }
        }
    }
    out.join(" ")
}

/// (pid, ppid, start_epoch_secs, comm)。starttime 是「开机后多少 tick」，
/// 加 `/proc/stat` 的 btime 才是 epoch —— 这一步漏了 age 会变成 50 多年。
#[cfg(any(target_os = "linux", feature = "sim_linux"))]
pub fn proc_bsdinfo(pid: i32) -> Option<(i32, i32, u64, String)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let info = linux::proc_stat(&stat)?;
    let btime = std::fs::read_to_string("/proc/stat")
        .ok()
        .and_then(|t| linux::stat_btime(&t))
        .unwrap_or(0);
    let started = btime.saturating_add(info.starttime / clk_tck());
    let name = std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|s| s.trim_end_matches(['\n', '\r']).to_string())
        .unwrap_or_default();
    Some((pid, info.ppid, started, name))
}

/// (磁盘读字节, 磁盘写字节, 能耗纳焦)。
///
/// 与 macOS 版**同形**：前两项是 I/O 累计字节，第三项是能耗。
/// Linux 没有 `ri_energy_nj` 的对应物，第三项恒为 0（不是「测到 0」，是「没有这个口径」）；
/// 调用方 `procs_state` 里它只进一个旁路字段。
#[cfg(any(target_os = "linux", feature = "sim_linux"))]
pub fn proc_rusage(pid: i32) -> Option<(u64, u64, u64)> {
    let io = std::fs::read_to_string(format!("/proc/{pid}/io")).ok()?;
    let r = linux::io_bytes(&io, "read_bytes")?;
    let w = linux::io_bytes(&io, "write_bytes")?;
    Some((r, w, 0))
}

/// 开机时刻（epoch 秒），取自 `/proc/stat` 的 btime。
#[cfg(any(target_os = "linux", feature = "sim_linux"))]
pub fn boot_time_secs() -> Option<f64> {
    let text = std::fs::read_to_string("/proc/stat").ok()?;
    linux::stat_btime(&text).map(|s| s as f64)
}

// ------------------------------------------------------------------ Windows（kernel32 / ntdll）
//
// 不引任何 crate：自己 `extern "system"` 声明要用的几个函数。
// 只在 `cargo check --features sim_windows` 下做类型检查（macOS 上不链接，
// 所以这里的 extern 符号不会去找）。结构体一律 `#[repr(C)]`、字段顺序照 Win32 文档。

#[cfg(any(windows, feature = "sim_windows"))]
mod win {
    #![allow(non_snake_case, non_camel_case_types)]
    use std::os::raw::{c_int, c_void};

    pub type HANDLE = *mut c_void;
    pub type BOOL = c_int;
    pub type DWORD = u32;
    pub type LONG = i32;
    pub type SIZE_T = usize;
    pub type ULONG_PTR = usize;
    pub type WCHAR = u16;
    pub type LPCWSTR = *const u16;

    pub const INVALID_HANDLE_VALUE: HANDLE = -1isize as HANDLE;
    pub const PROCESS_QUERY_LIMITED_INFORMATION: DWORD = 0x1000;
    pub const PROCESS_VM_READ: DWORD = 0x0010;
    pub const TH32CS_SNAPPROCESS: DWORD = 0x0000_0002;
    pub const MAX_PATH_W: usize = 260;

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    pub struct FileTime {
        pub dwLowDateTime: DWORD,
        pub dwHighDateTime: DWORD,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct ProcessMemoryCounters {
        pub cb: DWORD,
        pub PageFaultCount: DWORD,
        pub PeakWorkingSetSize: SIZE_T,
        pub WorkingSetSize: SIZE_T,
        pub QuotaPeakPagedPoolUsage: SIZE_T,
        pub QuotaPagedPoolUsage: SIZE_T,
        pub QuotaPeakNonPagedPoolUsage: SIZE_T,
        pub QuotaNonPagedPoolUsage: SIZE_T,
        pub PagefileUsage: SIZE_T,
        pub PeakPagefileUsage: SIZE_T,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct MemoryStatusEx {
        pub dwLength: DWORD,
        pub dwMemoryLoad: DWORD,
        pub ullTotalPhys: u64,
        pub ullAvailPhys: u64,
        pub ullTotalPageFile: u64,
        pub ullAvailPageFile: u64,
        pub ullTotalVirtual: u64,
        pub ullAvailVirtual: u64,
        pub ullAvailExtendedVirtual: u64,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct SystemInfo {
        pub wProcessorArchitecture: u16,
        pub wReserved: u16,
        pub dwPageSize: DWORD,
        pub lpMinimumApplicationAddress: *mut c_void,
        pub lpMaximumApplicationAddress: *mut c_void,
        pub dwActiveProcessorMask: ULONG_PTR,
        pub dwNumberOfProcessors: DWORD,
        pub dwProcessorType: DWORD,
        pub dwAllocationGranularity: DWORD,
        pub wProcessorLevel: u16,
        pub wProcessorRevision: u16,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct ProcessEntry32W {
        pub dwSize: DWORD,
        pub cntUsage: DWORD,
        pub th32ProcessID: DWORD,
        pub th32DefaultHeapID: ULONG_PTR,
        pub th32ModuleID: DWORD,
        pub cntThreads: DWORD,
        pub th32ParentProcessID: DWORD,
        pub pcPriClassBase: LONG,
        pub dwFlags: DWORD,
        pub szExeFile: [WCHAR; MAX_PATH_W],
    }

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    pub struct IoCounters {
        pub ReadOperationCount: u64,
        pub WriteOperationCount: u64,
        pub OtherOperationCount: u64,
        pub ReadTransferCount: u64,
        pub WriteTransferCount: u64,
        pub OtherTransferCount: u64,
    }

    /// `PROCESS_BASIC_INFORMATION`（NtQueryInformationProcess 的 class 0）。
    /// 只读 `InheritedFromUniqueProcessId` 拿 ppid，不碰 PEB。
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct ProcessBasicInformation {
        pub Reserved1: *mut c_void,
        pub PebBaseAddress: *mut c_void,
        pub Reserved2: [*mut c_void; 2],
        pub UniqueProcessId: ULONG_PTR,
        pub InheritedFromUniqueProcessId: ULONG_PTR,
    }

    #[link(name = "kernel32")]
    extern "system" {
        pub fn OpenProcess(access: DWORD, inherit: BOOL, pid: DWORD) -> HANDLE;
        pub fn CloseHandle(h: HANDLE) -> BOOL;
        pub fn TerminateProcess(h: HANDLE, code: u32) -> BOOL;
        pub fn K32GetProcessMemoryInfo(
            process: HANDLE,
            counters: *mut ProcessMemoryCounters,
            cb: DWORD,
        ) -> BOOL;
        pub fn GetProcessTimes(
            h: HANDLE,
            creation: *mut FileTime,
            exit: *mut FileTime,
            kernel: *mut FileTime,
            user: *mut FileTime,
        ) -> BOOL;
        pub fn GetProcessIoCounters(h: HANDLE, counters: *mut IoCounters) -> BOOL;
        pub fn QueryFullProcessImageNameW(
            h: HANDLE,
            flags: DWORD,
            buf: *mut WCHAR,
            size: *mut DWORD,
        ) -> BOOL;
        pub fn CreateToolhelp32Snapshot(flags: DWORD, pid: DWORD) -> HANDLE;
        pub fn Process32FirstW(snap: HANDLE, entry: *mut ProcessEntry32W) -> BOOL;
        pub fn Process32NextW(snap: HANDLE, entry: *mut ProcessEntry32W) -> BOOL;
        pub fn GetSystemTimes(idle: *mut FileTime, kernel: *mut FileTime, user: *mut FileTime)
            -> BOOL;
        pub fn GetSystemInfo(info: *mut SystemInfo);
        pub fn GlobalMemoryStatusEx(mem: *mut MemoryStatusEx) -> BOOL;
        pub fn GetTickCount64() -> u64;
        pub fn GetDiskFreeSpaceExW(
            path: LPCWSTR,
            caller_free: *mut u64,
            total: *mut u64,
            total_free: *mut u64,
        ) -> BOOL;
    }

    #[link(name = "ntdll")]
    extern "system" {
        pub fn NtQueryInformationProcess(
            process: HANDLE,
            class: u32,
            info: *mut c_void,
            len: u32,
            ret: *mut u32,
        ) -> i32;
    }

    pub fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn ft(v: FileTime) -> u64 {
        ((v.dwHighDateTime as u64) << 32) | v.dwLowDateTime as u64
    }

    pub fn open(pid: i32) -> Option<HANDLE> {
        let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ, 0, pid as DWORD) };
        if h.is_null() {
            None
        } else {
            Some(h)
        }
    }

    pub fn image_name(h: HANDLE) -> Option<String> {
        let mut buf = vec![0u16; MAX_PATH_W * 4];
        let mut size = buf.len() as DWORD;
        let ok = unsafe { QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut size) };
        if ok == 0 || size == 0 {
            return None;
        }
        Some(String::from_utf16_lossy(&buf[..size as usize]))
    }

    pub fn basename(path: &str) -> String {
        path.rsplit(['\\', '/']).next().unwrap_or(path).to_string()
    }

    pub fn sysinfo() -> SystemInfo {
        let mut si: SystemInfo = unsafe { std::mem::zeroed() };
        unsafe { GetSystemInfo(&mut si) };
        si
    }

    /// 扫一遍 toolhelp 快照找一个进程的 exe 名（`QueryFullProcessImageNameW` 失败时的兜底）。
    pub fn name_of(pid: i32) -> Option<String> {
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snap == INVALID_HANDLE_VALUE || snap.is_null() {
                return None;
            }
            let mut ent: ProcessEntry32W = std::mem::zeroed();
            ent.dwSize = std::mem::size_of::<ProcessEntry32W>() as DWORD;
            let mut found = None;
            let mut ok = Process32FirstW(snap, &mut ent);
            while ok != 0 {
                if ent.th32ProcessID == pid as DWORD {
                    let end = ent
                        .szExeFile
                        .iter()
                        .position(|c| *c == 0)
                        .unwrap_or(MAX_PATH_W);
                    found = Some(String::from_utf16_lossy(&ent.szExeFile[..end]));
                    break;
                }
                ok = Process32NextW(snap, &mut ent);
            }
            CloseHandle(snap);
            found
        }
    }
}

#[cfg(any(windows, feature = "sim_windows"))]
pub fn page_size() -> u64 {
    let p = win::sysinfo().dwPageSize;
    if p > 0 {
        p as u64
    } else {
        4096
    }
}

#[cfg(any(windows, feature = "sim_windows"))]
pub fn cpu_count() -> i64 {
    let n = win::sysinfo().dwNumberOfProcessors;
    if n > 0 {
        n as i64
    } else {
        1
    }
}

/// `GlobalMemoryStatusEx` 的 `ullTotalPhys`（psutil 的 total 同此）。
#[cfg(any(windows, feature = "sim_windows"))]
pub fn mem_total() -> Option<u64> {
    let mut m: win::MemoryStatusEx = unsafe { std::mem::zeroed() };
    m.dwLength = std::mem::size_of::<win::MemoryStatusEx>() as u32;
    if unsafe { win::GlobalMemoryStatusEx(&mut m) } == 0 {
        return None;
    }
    Some(m.ullTotalPhys)
}

/// (free, active, inactive, wired, speculative) 页数 —— Windows 只有可用物理内存。
///
/// 与 Linux 分支同一套归法：`ullAvailPhys` 折成 free 页、其余给 0，
/// 让 `mem()` 的 `avail` 等于 psutil 的 `available`（`ullAvailPhys`）。
#[cfg(any(windows, feature = "sim_windows"))]
pub fn vm_counts() -> Option<(u64, u64, u64, u64, u64)> {
    let mut m: win::MemoryStatusEx = unsafe { std::mem::zeroed() };
    m.dwLength = std::mem::size_of::<win::MemoryStatusEx>() as u32;
    if unsafe { win::GlobalMemoryStatusEx(&mut m) } == 0 {
        return None;
    }
    Some((m.ullAvailPhys / page_size().max(1), 0, 0, 0, 0))
}

/// `GetSystemTimes` 的 (user, system, idle, nice)。
///
/// 三个 FILETIME 都是 100ns 累计值。内核时间**含 idle**，psutil 的 system
/// 不含，所以要减掉；Windows 没有 nice 的口径，第 4 位恒 0。
#[cfg(any(windows, feature = "sim_windows"))]
pub fn cpu_ticks() -> Option<(u64, u64, u64, u64)> {
    let (mut idle, mut kern, mut user) = (
        win::FileTime::default(),
        win::FileTime::default(),
        win::FileTime::default(),
    );
    if unsafe { win::GetSystemTimes(&mut idle, &mut kern, &mut user) } == 0 {
        return None;
    }
    let idle = win::ft(idle);
    let kern = win::ft(kern);
    let user = win::ft(user);
    Some((user, kern.saturating_sub(idle), idle, 0))
}

/// 全部 pid（toolhelp 快照），升序。
#[cfg(any(windows, feature = "sim_windows"))]
pub fn list_pids() -> Vec<i32> {
    let mut out = Vec::new();
    unsafe {
        let snap = win::CreateToolhelp32Snapshot(win::TH32CS_SNAPPROCESS, 0);
        if snap == win::INVALID_HANDLE_VALUE || snap.is_null() {
            return out;
        }
        let mut ent: win::ProcessEntry32W = std::mem::zeroed();
        ent.dwSize = std::mem::size_of::<win::ProcessEntry32W>() as u32;
        let mut ok = win::Process32FirstW(snap, &mut ent);
        while ok != 0 {
            if ent.th32ProcessID > 0 {
                out.push(ent.th32ProcessID as i32);
            }
            ok = win::Process32NextW(snap, &mut ent);
        }
        win::CloseHandle(snap);
    }
    out.sort_unstable();
    out
}

/// (rss 字节, 累计 user+system 微秒)。
///
/// rss 用 `WorkingSetSize` —— psutil 在 Windows 上的 `rss` 就是这个字段。
/// CPU 用 `GetProcessTimes`（user+kernel，100ns → /10 得微秒）；
/// 注意 `GetTickCount64` 是**系统开机时长**，不是单个进程的 CPU，替代不了这一步。
#[cfg(any(windows, feature = "sim_windows"))]
pub fn proc_taskinfo(pid: i32) -> Option<(u64, u64)> {
    let h = win::open(pid)?;
    let mut pmc: win::ProcessMemoryCounters = unsafe { std::mem::zeroed() };
    pmc.cb = std::mem::size_of::<win::ProcessMemoryCounters>() as u32;
    let mem_ok =
        unsafe { win::K32GetProcessMemoryInfo(h, &mut pmc, pmc.cb) } != 0;
    let (mut c, mut e, mut k, mut u) = (
        win::FileTime::default(),
        win::FileTime::default(),
        win::FileTime::default(),
        win::FileTime::default(),
    );
    let time_ok = unsafe { win::GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u) } != 0;
    unsafe { win::CloseHandle(h) };
    // 任一项取不到就整项给 None：rss 有、CPU 编 0 会让调用方以为进程没在用 CPU。
    if !mem_ok || !time_ok {
        return None;
    }
    Some((
        pmc.WorkingSetSize as u64,
        (win::ft(k) + win::ft(u)) / 10,
    ))
}

/// `QueryFullProcessImageNameW` 拿完整路径。
#[cfg(any(windows, feature = "sim_windows"))]
pub fn proc_path(pid: i32) -> Option<String> {
    let h = win::open(pid)?;
    let p = win::image_name(h);
    unsafe { win::CloseHandle(h) };
    p
}

/// Windows 上不做打开文件枚举：`NtQuerySystemInformation` 的 handle 表要额外
/// 结构体与权限，收益只在「认脚本进程属主」这一条线上。Python 侧由 psutil 提供，
/// Rust 这版如实返回空串，不影响 name/ppid/rss/argv。
#[cfg(any(windows, feature = "sim_windows"))]
pub fn proc_fd_paths(_pid: i32, _limit: usize) -> String {
    String::new()
}

/// (pid, ppid, start_epoch_secs, name)。
///
/// ppid 走 `NtQueryInformationProcess(ProcessBasicInformation)` 的
/// `InheritedFromUniqueProcessId`（只取这一个字段，不读 PEB）。
/// name 取 exe 文件名（psutil 的 `name()` 在 Windows 上就是它），
/// 路径拿不到时退 toolhelp 快照里的 `szExeFile`。
#[cfg(any(windows, feature = "sim_windows"))]
pub fn proc_bsdinfo(pid: i32) -> Option<(i32, i32, u64, String)> {
    let h = win::open(pid)?;
    let mut pbi: win::ProcessBasicInformation = unsafe { std::mem::zeroed() };
    let mut ret: u32 = 0;
    let nt = unsafe {
        win::NtQueryInformationProcess(
            h,
            0,
            &mut pbi as *mut _ as *mut std::ffi::c_void,
            std::mem::size_of::<win::ProcessBasicInformation>() as u32,
            &mut ret,
        )
    };
    let (mut c, mut e, mut k, mut u) = (
        win::FileTime::default(),
        win::FileTime::default(),
        win::FileTime::default(),
        win::FileTime::default(),
    );
    let time_ok = unsafe { win::GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u) } != 0;
    let name = win::image_name(h)
        .map(|p| win::basename(&p))
        .or_else(|| win::name_of(pid))
        .unwrap_or_default();
    unsafe { win::CloseHandle(h) };
    if nt != 0 || !time_ok {
        return None;
    }
    // FILETIME 是 1601-01-01 起的 100ns；换成 Unix epoch 秒。
    let started = (win::ft(c) / 10_000_000).saturating_sub(11_644_473_600);
    Some((
        pid,
        pbi.InheritedFromUniqueProcessId as i32,
        started,
        name,
    ))
}

/// (磁盘读字节, 磁盘写字节, 能耗纳焦)。
///
/// 前两项来自 `GetProcessIoCounters`（`ReadTransferCount` / `WriteTransferCount`），
/// 语义是「本进程发起的读写传输量」，比 macOS 的 `ri_diskio_bytes*` 稍宽（含网络/设备）。
/// 能耗没有 Windows 对应物，第三项恒 0 —— 是「没有这个口径」，不是「测到 0」。
#[cfg(any(windows, feature = "sim_windows"))]
pub fn proc_rusage(pid: i32) -> Option<(u64, u64, u64)> {
    let h = win::open(pid)?;
    let mut io: win::IoCounters = win::IoCounters::default();
    let ok = unsafe { win::GetProcessIoCounters(h, &mut io) } != 0;
    unsafe { win::CloseHandle(h) };
    if !ok {
        return None;
    }
    Some((io.ReadTransferCount, io.WriteTransferCount, 0))
}

/// 开机时刻（epoch 秒）：`GetTickCount64` 是开机以来的毫秒数。
#[cfg(any(windows, feature = "sim_windows"))]
pub fn boot_time_secs() -> Option<f64> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .ok()?;
    let up = unsafe { win::GetTickCount64() } as f64 / 1000.0;
    Some(now - up)
}

// ------------------------------------------------------------------ 进程存活/终止
//
// 原来这两件事散在几个心跳/灰测文件里直接写 `libc::kill`，Windows 上编不过。
// 统一收到平台层：语义只有「还活着吗」「让它退出」。

/// 进程是否还活着。POSIX 用 `kill(pid, 0)`（不发信号，只做存在性与权限检查），
/// Windows 用 `OpenProcess` 拿句柄 —— 拿得到就是活的。
///
/// `sim_windows` 是「在 mac 上按 windows 语义过一遍类型」的开关，
/// 所以 unix 分支必须把它排除掉，否则同一个 mac 构建里两套实现同时存在。
#[cfg(all(unix, not(feature = "sim_windows")))]
pub fn proc_alive(pid: i64) -> bool {
    if pid <= 0 {
        return false;
    }
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(any(windows, feature = "sim_windows"))]
pub fn proc_alive(pid: i64) -> bool {
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    if pid <= 0 {
        return false;
    }
    let h = unsafe { win::OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid as u32) };
    if h.is_null() {
        return false;
    }
    unsafe { win::CloseHandle(h) };
    true
}

#[cfg(all(not(unix), not(windows), not(feature = "sim_windows")))]
pub fn proc_alive(_pid: i64) -> bool {
    false
}

/// 要求进程退出（不等待）。POSIX 发 SIGTERM，Windows 用 `TerminateProcess`。
/// 调用方自己负责轮询确认，这里不阻塞。
#[cfg(all(unix, not(feature = "sim_windows")))]
pub fn terminate(pid: i64) -> bool {
    if pid <= 0 {
        return false;
    }
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) == 0 }
}

#[cfg(any(windows, feature = "sim_windows"))]
pub fn terminate(pid: i64) -> bool {
    const PROCESS_TERMINATE: u32 = 0x0001;
    if pid <= 0 {
        return false;
    }
    let h = unsafe { win::OpenProcess(PROCESS_TERMINATE, 0, pid as u32) };
    if h.is_null() {
        return false;
    }
    let ok = unsafe { win::TerminateProcess(h, 1) } != 0;
    unsafe { win::CloseHandle(h) };
    ok
}

#[cfg(all(not(unix), not(windows), not(feature = "sim_windows")))]
pub fn terminate(_pid: i64) -> bool {
    false
}

// ------------------------------------------------------------------ 可执行文件
//
// 「这个文件能不能直接跑」在 POSIX 看 mode 位，在 Windows 看扩展名。
// `shutil.which()`、找 CLI（ollama/ffmpeg/tailscale）到处都要这个判断。

/// 能不能当可执行文件跑：普通文件 + 有执行位（Windows：扩展名在 PATHEXT 里）。
#[cfg(all(unix, not(feature = "sim_windows")))]
pub fn is_exec_file(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(p) {
        Ok(m) => m.is_file() && (m.permissions().mode() & 0o111) != 0,
        Err(_) => false,
    }
}

#[cfg(any(windows, feature = "sim_windows"))]
pub fn is_exec_file(p: &Path) -> bool {
    let md = match std::fs::metadata(p) {
        Ok(m) => m,
        Err(_) => return false,
    };
    if !md.is_file() {
        return false;
    }
    // PATHEXT 是 Windows 的「后缀表」，用户可能改过；读不到就用系统默认那几个。
    let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
    let ext = p
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy().to_uppercase()))
        .unwrap_or_default();
    !ext.is_empty()
        && pathext
            .split(';')
            .any(|x| x.trim().eq_ignore_ascii_case(ext.as_str()))
}

/// 给文件补执行位（`chmod +x`）。Windows 没有这个位，空实现。
#[cfg(all(unix, not(feature = "sim_windows")))]
pub fn add_exec_bits(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(md) = std::fs::metadata(p) {
        let mode = md.permissions().mode();
        let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode | 0o111));
    }
}

#[cfg(any(windows, feature = "sim_windows", not(any(unix, windows))))]
pub fn add_exec_bits(_p: &Path) {}

// ------------------------------------------------------------------ 负载
//
/// `os.getloadavg()`。Windows 没有这个口径 —— 返回 `None`，不是 `[0,0,0]`
/// （0 看起来像「机器很闲」，与「没这个指标」是两件事，调用方要能区分）。
#[cfg(all(unix, not(feature = "sim_windows")))]
pub fn load_avg() -> Option<[f64; 3]> {
    let mut v = [0f64; 3];
    let n = unsafe { libc::getloadavg(v.as_mut_ptr(), 3) };
    if n == 3 {
        Some(v)
    } else {
        None
    }
}

#[cfg(any(windows, feature = "sim_windows", not(any(unix, windows))))]
pub fn load_avg() -> Option<[f64; 3]> {
    None
}

// ------------------------------------------------------------------ 文件元数据
//
// 增量扫（会话日志的小时桶、工作统计、聚合扫描）都靠 `stat` 的三个数判断
// 「还是不是上次那个文件、追加了多少」：ino / size / mtime。
// `size` 与 `mtime` 有可移植写法（`len()` / `modified()`），`ino` 没有，
// 所以统一收到这里，调用方不再直接 `use std::os::unix::fs::MetadataExt`。

/// 文件大小。等价于 Python 的 `st_size`。
pub fn file_size(md: &std::fs::Metadata) -> u64 {
    md.len()
}

/// `st_mtime` 的浮点秒。刻意按 `secs + nsec/1e9` 拼，与 Python 的
/// `st_mtime` 逐位一致（`as_secs_f64()` 内部就是这么算的，这里写开是为了
/// 在这几个文件的注释里说清楚口径）。
pub fn file_mtime(md: &std::fs::Metadata) -> f64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// `st_ino`：只用来判断「同一个路径上是不是换了个文件」。
///
/// - POSIX：真 inode 号。
/// - Windows：退一步用「最后写入时间」（FILETIME）。
///   `MetadataExt::file_index()` 至今仍是 nightly（feature `windows_by_handle`），
///   stable 上编不过 —— 曾经的实现就卡在这里，Windows 包因此发不出去。
///   换成 last_write_time 是**安全方向**：同一文件被追加或替换时它必变，
///   宁可多重扫一次，也绝不会把两个不同文件当成同一个续读。
///   （恒返回 0 会踩那个坑：`prev_ino == ino` 恒成立，新文件更大时会被当成旧文件接着读。）
/// - `sim_windows`：在 mac 上过类型，拿不到 Windows 的 `MetadataExt`，
///   返回 0 —— 只在跑 `--features sim_windows` 的类型检查时出现，不影响真实平台。
#[cfg(all(unix, not(feature = "sim_windows")))]
pub fn file_inode(md: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    md.ino()
}

#[cfg(windows)]
pub fn file_inode(md: &std::fs::Metadata) -> u64 {
    use std::os::windows::fs::MetadataExt;
    md.last_write_time()
}

#[cfg(any(feature = "sim_windows", not(any(unix, windows))))]
pub fn file_inode(_md: &std::fs::Metadata) -> u64 {
    0
}

// ------------------------------------------------------------------ 磁盘

/// `shutil.disk_usage` / `os.statvfs` 口径：(total, free)。
/// Linux 与 macOS 走同一条 statvfs（`f_bavail` = 普通用户可用 = Python 的 free）。
#[cfg(all(unix, not(feature = "sim_windows")))]
pub fn disk_usage(p: &Path) -> Option<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(p.as_os_str().as_bytes()).ok()?;
    unsafe {
        let mut st: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c.as_ptr(), &mut st) != 0 {
            return None;
        }
        let frsize = if st.f_frsize != 0 { st.f_frsize } else { st.f_bsize };
        let total = (st.f_blocks as u64).saturating_mul(frsize as u64);
        let free = (st.f_bavail as u64).saturating_mul(frsize as u64);
        Some((total, free))
    }
}

/// Windows：`GetDiskFreeSpaceExW`。`lpFreeBytesAvailableToCaller` 对应
/// `shutil.disk_usage().free`（配额内可用），`lpTotalNumberOfBytes` 对应 total。
#[cfg(any(windows, feature = "sim_windows"))]
pub fn disk_usage(p: &Path) -> Option<(u64, u64)> {
    let w = win::wide(&p.to_string_lossy());
    let (mut avail, mut total, mut total_free) = (0u64, 0u64, 0u64);
    let ok = unsafe { win::GetDiskFreeSpaceExW(w.as_ptr(), &mut avail, &mut total, &mut total_free) };
    if ok == 0 {
        return None;
    }
    Some((total, avail))
}

#[cfg(all(not(unix), not(windows), not(feature = "sim_windows")))]
pub fn disk_usage(_p: &Path) -> Option<(u64, u64)> {
    None
}

// ------------------------------------------------------------------ 网络

/// `_port(port, host, timeout)`：能连上就算活。
pub fn port_open(port: u16, host: &str, timeout: Duration) -> bool {
    use std::net::{TcpStream, ToSocketAddrs};
    let addr = match (host, port).to_socket_addrs() {
        Ok(mut it) => match it.next() {
            Some(a) => a,
            None => return false,
        },
        Err(_) => return false,
    };
    TcpStream::connect_timeout(&addr, timeout).is_ok()
}

/// 本机所有 IPv4 地址（getifaddrs）。`psutil.net_if_addrs()` 的等价物。
pub fn iface_ips() -> Vec<String> {
    let mut out = Vec::new();
    #[cfg(unix)]
    unsafe {
        let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut head) != 0 {
            return out;
        }
        let mut cur = head;
        while !cur.is_null() {
            let a = (*cur).ifa_addr;
            if !a.is_null() && (*a).sa_family as i32 == libc::AF_INET {
                let sin = a as *const libc::sockaddr_in;
                let ip = u32::from_be((*sin).sin_addr.s_addr);
                out.push(format!(
                    "{}.{}.{}.{}",
                    (ip >> 24) & 0xff,
                    (ip >> 16) & 0xff,
                    (ip >> 8) & 0xff,
                    ip & 0xff
                ));
            }
            cur = (*cur).ifa_next;
        }
        libc::freeifaddrs(head);
    }
    out
}

/// 主机名（不含域）。
#[cfg(all(unix, not(feature = "sim_windows")))]
pub fn hostname_short() -> String {
    let mut buf = vec![0u8; 256];
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if rc != 0 {
        return "localhost".to_string();
    }
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    let name = String::from_utf8_lossy(&buf[..end]).to_string();
    name.split('.').next().unwrap_or("localhost").to_string()
}

/// Windows：`COMPUTERNAME` 就是 NetBIOS 主机名（不带域）。
#[cfg(any(windows, feature = "sim_windows"))]
pub fn hostname_short() -> String {
    std::env::var("COMPUTERNAME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "localhost".to_string())
}

#[cfg(all(not(unix), not(windows), not(feature = "sim_windows")))]
pub fn hostname_short() -> String {
    "localhost".to_string()
}

// ------------------------------------------------------------------ 子进程

/// 跑一条命令，返回 stdout（超时/失败给空串）。Python `_run` 的口径。
pub fn run_cmd(argv: &[&str], timeout: Duration) -> String {
    // 走带读线程的那条：`launchctl print` / `system_profiler` 的输出都远超
    // 64 KB 的管道缓冲，先等退出再读会死锁，最后超时返回空串。
    run_cmd_piped(argv, timeout).1
}

/// 同 `run_cmd_piped`，但 stdout 保留**原始字节**（ioreg 吐的是二进制 plist）。
pub fn run_cmd_raw(argv: &[&str], timeout: Duration) -> Vec<u8> {
    use std::io::Read;
    let mut cmd = std::process::Command::new(argv[0]);
    cmd.args(&argv[1..]);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::null());
    cmd.stdin(std::process::Stdio::null());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut so = child.stdout.take();
    let t_out = std::thread::spawn(move || {
        let mut b = Vec::new();
        if let Some(h) = so.as_mut() {
            let _ = h.read_to_end(&mut b);
        }
        b
    });
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Vec::new();
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return Vec::new(),
        }
    }
    t_out.join().unwrap_or_default()
}

/// 跑一条命令，返回 (returncode, stdout, stderr)。
pub fn run_cmd_full(argv: &[&str], timeout: Duration) -> (i32, String, String) {
    use std::io::Read;
    let mut cmd = std::process::Command::new(argv[0]);
    cmd.args(&argv[1..]);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.stdin(std::process::Stdio::null());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => return (-1, String::new(), String::new()),
    };
    let start = Instant::now();
    let mut code = -1i32;
    loop {
        match child.try_wait() {
            Ok(Some(st)) => {
                code = st.code().unwrap_or(0);
                break;
            }
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return (-1, String::new(), String::new());
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return (-1, String::new(), String::new()),
        }
    }
    let mut so = String::new();
    let mut se = String::new();
    if let Some(mut h) = child.stdout.take() {
        let mut b = Vec::new();
        let _ = h.read_to_end(&mut b);
        so = String::from_utf8_lossy(&b).to_string();
    }
    if let Some(mut h) = child.stderr.take() {
        let mut b = Vec::new();
        let _ = h.read_to_end(&mut b);
        se = String::from_utf8_lossy(&b).to_string();
    }
    (code, so, se)
}

/// 同 `run_cmd_full`，但**先起两条读线程再等退出**：输出大于管道缓冲也不会死锁。
pub fn run_cmd_piped(argv: &[&str], timeout: Duration) -> (i32, String, String) {
    use std::io::Read;
    let mut cmd = std::process::Command::new(argv[0]);
    cmd.args(&argv[1..]);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.stdin(std::process::Stdio::null());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => return (-1, String::new(), String::new()),
    };
    let mut so_h = child.stdout.take();
    let mut se_h = child.stderr.take();
    let t_out = std::thread::spawn(move || {
        let mut b = Vec::new();
        if let Some(h) = so_h.as_mut() {
            let _ = h.read_to_end(&mut b);
        }
        String::from_utf8_lossy(&b).to_string()
    });
    let t_err = std::thread::spawn(move || {
        let mut b = Vec::new();
        if let Some(h) = se_h.as_mut() {
            let _ = h.read_to_end(&mut b);
        }
        String::from_utf8_lossy(&b).to_string()
    });
    let start = Instant::now();
    let mut code = -1i32;
    loop {
        match child.try_wait() {
            Ok(Some(st)) => {
                code = st.code().unwrap_or(0);
                break;
            }
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return (-1, String::new(), String::new());
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return (-1, String::new(), String::new()),
        }
    }
    let so = t_out.join().unwrap_or_default();
    let se = t_err.join().unwrap_or_default();
    (code, so, se)
}

// ------------------------------------------------------------------ sqlite（只读，走 CLI）

/// 用 sqlite3 命令行只读查一份库，返回行的 JSON 数组。
/// Python 侧是 `sqlite3.connect("file:...?mode=ro")`，读的是同一份文件、同一套 SQL。
///
/// 输出可能上兆（Cherry 的 ai_usage_record 一次 470 KB），所以必须**边跑边读**：
/// 先 `try_wait` 再读管道会撞上 64 KB 的管道缓冲，子进程写满就卡住，最后超时返回空。
pub fn sqlite_rows(db: &Path, sql: &str, timeout: Duration) -> Option<Vec<serde_json::Value>> {
    let path = db.to_string_lossy().to_string();
    let (code, out, _err) = run_cmd_piped(
        &["sqlite3", "-readonly", "-json", &path, sql],
        timeout,
    );
    if code != 0 {
        return None;
    }
    let text = out.trim();
    if text.is_empty() {
        return Some(Vec::new());
    }
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(serde_json::Value::Array(a)) => Some(a),
        _ => None,
    }
}

// ------------------------------------------------------------------ 解析单测
//
// 这些是这次唯一能在 macOS 上真执行的证据：`cargo test` 跑 parse_cmdline，
// `cargo test --features sim_linux` 再跑 /proc 的四个解析器（纯文本，不碰系统调用）。
#[cfg(test)]
mod tests {
    #[test]
    fn cmdline_splits_on_nul_and_drops_empty() {
        let raw = b"/usr/bin/python3\0-u\0/app/main.py\0\0";
        assert_eq!(
            super::parse_cmdline(raw),
            vec!["/usr/bin/python3", "-u", "/app/main.py"]
        );
        assert!(super::parse_cmdline(b"").is_empty());
    }

    #[cfg(any(target_os = "linux", feature = "sim_linux"))]
    mod procfs {
        use super::super::linux;

        #[test]
        fn meminfo_reads_kb_and_ignores_other_keys() {
            let text = "\
MemTotal:       16316372 kB
MemFree:          234567 kB
MemAvailable:    8123456 kB
Buffers:           12345 kB
Cached:          4567890 kB
";
            assert_eq!(linux::meminfo_kb(text, "MemTotal"), Some(16316372));
            assert_eq!(linux::meminfo_kb(text, "MemAvailable"), Some(8123456));
            assert_eq!(linux::meminfo_kb(text, "Buffers"), Some(12345));
            assert_eq!(linux::meminfo_kb(text, "SwapTotal"), None);
            // 前缀相同但键不同，不能被 "MemFreeX" 之类误命中
            assert_eq!(linux::meminfo_kb("MemFreeX: 9 kB\n", "MemFree"), None);
        }

        #[test]
        fn proc_stat_cpu_is_reordered_like_macos() {
            let text = "\
cpu  100 20 300 4000 50 6 7 8 0 0
cpu0 1 2 3 4 5 6 7 8 0 0
";
            // /proc/stat: user=100 nice=20 system=300 idle=4000
            // 返回 mac 序：user, system, idle, nice
            assert_eq!(linux::stat_cpu(text), Some((100, 300, 4000, 20)));
            assert_eq!(linux::stat_cpu("cpu0 1 2 3\n"), None);
        }

        #[test]
        fn proc_stat_btime() {
            let text = "cpu  1 2 3 4\nbtime 1700000000\nprocesses 12345\n";
            assert_eq!(linux::stat_btime(text), Some(1_700_000_000));
            assert_eq!(linux::stat_btime("cpu 1 2 3 4\n"), None);
        }

        #[test]
        fn pid_stat_survives_spaces_and_parens_in_comm() {
            // 真实样例：comm 是 "(node) server" 这种带括号的名字
            let text = "1234 (node) server) S 1000 1234 1234 0 -1 4194560 100 0 0 0 \
                        111 22 0 0 20 0 12 0 987654 123456789 4321 18446744073709551615 0 0 0 0 0 0 0 0 0 0 0 0 17 3 0 0 0 0 0";
            let st = linux::proc_stat(text).expect("parse");
            assert_eq!(st.ppid, 1000);
            assert_eq!(st.utime, 111);
            assert_eq!(st.stime, 22);
            assert_eq!(st.starttime, 987654);
        }

        #[test]
        fn statm_resident_second_field() {
            assert_eq!(linux::statm_resident("12345 678 90 1 0 12 0\n"), Some(678));
            assert_eq!(linux::statm_resident("12345\n"), None);
        }

        #[test]
        fn proc_io_bytes() {
            let text = "\
rchar: 111
wchar: 222
syscr: 3
syscw: 4
read_bytes: 55555
write_bytes: 66666
cancelled_write_bytes: 0
";
            assert_eq!(linux::io_bytes(text, "read_bytes"), Some(55555));
            assert_eq!(linux::io_bytes(text, "write_bytes"), Some(66666));
            assert_eq!(linux::io_bytes(text, "syscr"), Some(3));
            assert_eq!(linux::io_bytes(text, "read_bytesX"), None);
        }
    }
}
