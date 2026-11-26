// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：w5a 需要的平台能力 —— 内存/CPU/进程（libproc + mach）、磁盘用量、
//       端口探活、HTTP 探活、跑外部命令（带超时）、只读 sqlite 查询。
// 不做：不做业务判断。所有"取不到"都返回 None，由调用方决定回 null 还是空表。
//
// 为什么不用 ps / df / vm_stat 文本：Python 侧走的是 psutil（libproc + mach）
// 和 os.statvfs，解析命令行输出会被列宽、本地化、沙箱拦 ps 影响，一差就是整块数据。
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
    }

    pub const HOST_VM_INFO64: c_int = 4;
    pub const HOST_CPU_LOAD_INFO: c_int = 3;
    pub const PROC_PIDTASKINFO: c_int = 4;
    pub const PROC_PIDTBSDINFO: c_int = 3;
    pub const RUSAGE_INFO_V2: c_int = 2;

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
#[cfg(target_os = "macos")]
pub fn page_size() -> u64 {
    unsafe { libc::sysconf(libc::_SC_PAGESIZE) as u64 }
}

/// 物理内存总量（sysctl hw.memsize）。
#[cfg(target_os = "macos")]
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
#[cfg(target_os = "macos")]
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
#[cfg(target_os = "macos")]
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
#[cfg(target_os = "macos")]
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
        buf
    }
}

/// (rss 字节, 累计 user+system 微秒)
#[cfg(target_os = "macos")]
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

/// 进程可执行文件路径。
#[cfg(target_os = "macos")]
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

/// (pid, ppid, start_tvsec, comm)
#[cfg(target_os = "macos")]
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
        let end = bi.pbi_name.iter().position(|c| *c == 0).unwrap_or(16);
        let name: Vec<u8> = bi.pbi_name[..end].iter().map(|c| *c as u8).collect();
        Some((
            bi.pbi_pid as i32,
            bi.pbi_ppid as i32,
            bi.pbi_start_tvsec,
            String::from_utf8_lossy(&name).to_string(),
        ))
    }
}

// ------------------------------------------------------------------ 磁盘

/// `shutil.disk_usage` / `os.statvfs` 口径：(total, free)。
#[cfg(unix)]
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

#[cfg(not(unix))]
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

// ------------------------------------------------------------------ 子进程

/// 跑一条命令，返回 stdout（超时/失败给空串）。Python `_run` 的口径。
pub fn run_cmd(argv: &[&str], timeout: Duration) -> String {
    let mut cmd = std::process::Command::new(argv[0]);
    cmd.args(&argv[1..]);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::null());
    cmd.stdin(std::process::Stdio::null());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => return String::new(),
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
                    return String::new();
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return String::new(),
        }
    }
    let mut out = String::new();
    if let Some(mut so) = child.stdout.take() {
        use std::io::Read;
        let mut buf = Vec::new();
        let _ = so.read_to_end(&mut buf);
        out = String::from_utf8_lossy(&buf).to_string();
    }
    out
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

// ------------------------------------------------------------------ sqlite（只读，走 CLI）

/// 用 sqlite3 命令行只读查一份库，返回行的 JSON 数组。
/// Python 侧是 `sqlite3.connect("file:...?mode=ro")`，读的是同一份文件、同一套 SQL。
pub fn sqlite_rows(db: &Path, sql: &str, timeout: Duration) -> Option<Vec<serde_json::Value>> {
    let path = db.to_string_lossy().to_string();
    let (code, out, _err) = run_cmd_full(
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
