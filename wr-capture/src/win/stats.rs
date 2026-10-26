// SPDX-License-Identifier: GPL-3.0-or-later

//! Once a minute, gsr.log gets a `stats` line on how the encode loop keeps up
//! and a `usage` line each for wr-capture, the app that started it, and the
//! captured game: CPU, memory and GPU load. GPU numbers come from the same
//! performance counters Task Manager shows, summed over the process's engines
//! of a kind (3D, or any video engine) and over all GPUs.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use windows::Win32::Foundation::{CloseHandle, FILETIME};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Performance::{
    PDH_CSTATUS_VALID_DATA, PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY,
    PDH_MORE_DATA, PdhAddEnglishCounterW, PdhCollectQueryData, PdhGetFormattedCounterArrayW,
    PdhOpenQueryW,
};
use windows::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::SystemInformation::GetSystemTimeAsFileTime;
use windows::Win32::System::Threading::{
    GetProcessTimes, GetSystemTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::core::{HSTRING, PCWSTR};

use super::Shared;

const INTERVAL: Duration = Duration::from_secs(60);

/// Encode-loop counters since the last `stats` line.
#[derive(Default)]
pub struct FrameStats {
    /// Ticks that had a new frame from the game.
    fresh: AtomicU32,
    /// Ticks that repeated the previous frame (or black): the game drew
    /// slower than the recording rate, or capture stalled.
    repeated: AtomicU32,
    /// Ticks skipped because the loop fell behind.
    dropped: AtomicU32,
    work_us: AtomicU64,
    worst_us: AtomicU64,
}

impl FrameStats {
    pub fn encoded(&self, fresh: bool, work: Duration) {
        let counter = if fresh { &self.fresh } else { &self.repeated };
        counter.fetch_add(1, Ordering::Relaxed);
        let us = work.as_micros() as u64;
        self.work_us.fetch_add(us, Ordering::Relaxed);
        self.worst_us.fetch_max(us, Ordering::Relaxed);
    }

    pub fn dropped(&self, ticks: i64) {
        self.dropped.fetch_add(ticks as u32, Ordering::Relaxed);
    }

    fn line(&self) -> String {
        let fresh = self.fresh.swap(0, Ordering::Relaxed);
        let repeated = self.repeated.swap(0, Ordering::Relaxed);
        let dropped = self.dropped.swap(0, Ordering::Relaxed);
        let work_us = self.work_us.swap(0, Ordering::Relaxed);
        let worst_us = self.worst_us.swap(0, Ordering::Relaxed);
        let average = work_us as f64 / f64::from((fresh + repeated).max(1)) / 1000.0;
        format!(
            "fresh={fresh} repeated={repeated} dropped={dropped} work_ms_avg={average:.2} work_ms_max={:.2}",
            worst_us as f64 / 1000.0
        )
    }
}

pub fn run(shared: Arc<Shared>) {
    let own = std::process::id();
    let parent = parent_pid(own);
    let mut gpu = Gpu::open();
    if gpu.is_none() {
        eprintln!("stats: GPU counters unavailable");
    }
    let cores = std::thread::available_parallelism().map_or(1, usize::from) as f64;
    let mut system = system_times();
    // Per process: CPU time and wall time at the previous sample.
    let mut previous: HashMap<u32, (u64, u64)> = HashMap::new();
    loop {
        std::thread::sleep(INTERVAL);
        let gpu = gpu.as_mut().map(Gpu::sample).unwrap_or_default();
        let (idle, busy) = system_times();
        let system_cpu = percent(busy - system.1 - (idle - system.0), busy - system.1);
        system = (idle, busy);
        eprintln!(
            "stats at={} {} system_cpu={system_cpu:.1}",
            filetime(unsafe { GetSystemTimeAsFileTime() }) / 10_000_000 - 11_644_473_600,
            shared.frames.line()
        );
        let game = shared.target_pid.load(Ordering::Relaxed);
        for (name, pid) in [("wr-capture", own), ("app", parent), ("game", game)] {
            if pid == 0 {
                continue;
            }
            let Some((cpu, wall, created, ram)) = process_times(pid) else {
                continue;
            };
            // A process that started since the last sample counts from its start.
            let (cpu_before, wall_before) = previous
                .get(&pid)
                .copied()
                .filter(|(_, wall)| *wall >= created)
                .unwrap_or((0, created));
            previous.insert(pid, (cpu, wall));
            let cpu_percent = percent(cpu - cpu_before, wall - wall_before) / cores;
            let usage = gpu.get(&pid).copied().unwrap_or_default();
            eprintln!(
                "usage {name} pid={pid} cpu={cpu_percent:.1} ram_mb={} gpu_3d={:.1} gpu_video={:.1} gpu_dedicated_mb={} gpu_shared_mb={}",
                ram / (1024 * 1024),
                usage.engine_3d,
                usage.engine_video,
                usage.dedicated / (1024 * 1024),
                usage.shared / (1024 * 1024),
            );
        }
        previous.retain(|pid, _| [own, parent, game].contains(pid));
    }
}

fn percent(part: u64, whole: u64) -> f64 {
    part as f64 * 100.0 / whole.max(1) as f64
}

fn filetime(time: FILETIME) -> u64 {
    (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
}

/// Idle and idle-plus-busy time of all cores, in 100 ns.
fn system_times() -> (u64, u64) {
    let (mut idle, mut kernel, mut user) = Default::default();
    let _ = unsafe { GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)) };
    // Kernel time includes idle time.
    (filetime(idle), filetime(kernel) + filetime(user))
}

/// CPU time, now, creation time (all 100 ns) and working set bytes.
fn process_times(pid: u32) -> Option<(u64, u64, u64, usize)> {
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let (mut created, mut exited, mut kernel, mut user) = Default::default();
        let times = GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user);
        let mut memory = PROCESS_MEMORY_COUNTERS {
            cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            ..Default::default()
        };
        let _ = K32GetProcessMemoryInfo(process, &mut memory, memory.cb);
        let _ = CloseHandle(process);
        times.ok()?;
        Some((
            filetime(kernel) + filetime(user),
            filetime(GetSystemTimeAsFileTime()),
            filetime(created),
            memory.WorkingSetSize,
        ))
    }
}

fn parent_pid(pid: u32) -> u32 {
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return 0;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut parent = 0;
        let mut more = Process32FirstW(snapshot, &mut entry).is_ok();
        while more {
            if entry.th32ProcessID == pid {
                parent = entry.th32ParentProcessID;
                break;
            }
            more = Process32NextW(snapshot, &mut entry).is_ok();
        }
        let _ = CloseHandle(snapshot);
        parent
    }
}

#[derive(Clone, Copy, Default)]
struct GpuUsage {
    engine_3d: f64,
    engine_video: f64,
    dedicated: u64,
    shared: u64,
}

struct Gpu {
    query: PDH_HQUERY,
    engines: PDH_HCOUNTER,
    dedicated: PDH_HCOUNTER,
    shared: PDH_HCOUNTER,
}

impl Gpu {
    fn open() -> Option<Self> {
        unsafe {
            let mut query = PDH_HQUERY::default();
            if PdhOpenQueryW(PCWSTR::null(), 0, &mut query) != 0 {
                return None;
            }
            let add = |path: &str| {
                let mut counter = PDH_HCOUNTER::default();
                (PdhAddEnglishCounterW(query, &HSTRING::from(path), 0, &mut counter) == 0)
                    .then_some(counter)
            };
            let gpu = Self {
                engines: add(r"\GPU Engine(*)\Utilization Percentage")?,
                dedicated: add(r"\GPU Process Memory(*)\Dedicated Usage")?,
                shared: add(r"\GPU Process Memory(*)\Shared Usage")?,
                query,
            };
            // Utilization is a rate: the first collection only sets the start.
            PdhCollectQueryData(query);
            Some(gpu)
        }
    }

    /// Usage per process id since the previous sample.
    fn sample(&mut self) -> HashMap<u32, GpuUsage> {
        let mut usage: HashMap<u32, GpuUsage> = HashMap::new();
        if unsafe { PdhCollectQueryData(self.query) } != 0 {
            return usage;
        }
        // Instances look like `pid_1234_luid_0x0_0x1_phys_0_eng_0_engtype_3D`.
        for (instance, value) in values(self.engines) {
            let Some(pid) = instance_pid(&instance) else {
                continue;
            };
            let entry = usage.entry(pid).or_default();
            match instance.rsplit_once("engtype_").map(|(_, kind)| kind) {
                Some("3D") => entry.engine_3d += value,
                Some(kind) if kind.starts_with("Video") => entry.engine_video += value,
                _ => {}
            }
        }
        for (instance, value) in values(self.dedicated) {
            if let Some(pid) = instance_pid(&instance) {
                usage.entry(pid).or_default().dedicated += value as u64;
            }
        }
        for (instance, value) in values(self.shared) {
            if let Some(pid) = instance_pid(&instance) {
                usage.entry(pid).or_default().shared += value as u64;
            }
        }
        usage
    }
}

fn instance_pid(instance: &str) -> Option<u32> {
    instance
        .strip_prefix("pid_")?
        .split('_')
        .next()?
        .parse()
        .ok()
}

/// Every instance of a wildcard counter with valid data.
fn values(counter: PDH_HCOUNTER) -> Vec<(String, f64)> {
    let (mut bytes, mut count) = (0, 0);
    let status = unsafe {
        PdhGetFormattedCounterArrayW(counter, PDH_FMT_DOUBLE, &mut bytes, &mut count, None)
    };
    if status != PDH_MORE_DATA {
        return Vec::new();
    }
    let size = size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>();
    // Item names point into the same buffer, after the items.
    let mut buffer = vec![PDH_FMT_COUNTERVALUE_ITEM_W::default(); (bytes as usize).div_ceil(size)];
    let status = unsafe {
        PdhGetFormattedCounterArrayW(
            counter,
            PDH_FMT_DOUBLE,
            &mut bytes,
            &mut count,
            Some(buffer.as_mut_ptr()),
        )
    };
    if status != 0 {
        return Vec::new();
    }
    buffer[..count as usize]
        .iter()
        .filter(|item| item.FmtValue.CStatus == PDH_CSTATUS_VALID_DATA)
        .map(|item| unsafe {
            (
                item.szName.to_string().unwrap_or_default(),
                item.FmtValue.Anonymous.doubleValue,
            )
        })
        .collect()
}
