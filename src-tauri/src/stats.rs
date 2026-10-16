//! System stats engine: samples only while subscribed, pushes events to the dock window.
//! 1s interval; zero frontend polling.

use serde_json::json;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Mutex;
use sysinfo::{Networks, System};
use tauri::{AppHandle, Emitter, Manager};

pub struct StatsState {
    subscribers: AtomicI32,
    sys: Mutex<System>,
    nets: Mutex<Networks>,
    running: Mutex<bool>,
}

impl StatsState {
    pub fn new() -> Self {
        Self {
            subscribers: AtomicI32::new(0),
            sys: Mutex::new(System::new()),
            nets: Mutex::new(Networks::new_with_refreshed_list()),
            running: Mutex::new(false),
        }
    }

    pub fn subscribe(&self, app: &AppHandle) {
        if self.subscribers.fetch_add(1, Ordering::SeqCst) != 0 {
            return; // already running
        }
        {
            let mut running = self.running.lock().unwrap();
            if *running {
                return;
            }
            *running = true;
        }
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            // Warm-up so first sample is meaningful.
            {
                let state = app.state::<crate::AppState>();
                state.stats.sys.lock().unwrap().refresh_cpu_usage();
                state.stats.nets.lock().unwrap().refresh(true);
            }
            let mut prev_rx: u64 = 0;
            let mut prev_tx: u64 = 0;
            let mut tick_n = 0u32;
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;

                let app_state = app.state::<crate::AppState>();
                let state = &app_state.stats;
                if state.subscribers.load(Ordering::SeqCst) <= 0 {
                    println!("[stats] no subscribers, loop exit");
                    *state.running.lock().unwrap() = false;
                    break;
                }

                let (cpu, mem_used, mem_total) = {
                    let mut sys = state.sys.lock().unwrap();
                    sys.refresh_cpu_usage();
                    sys.refresh_memory();
                    (sys.global_cpu_usage(), sys.used_memory(), sys.total_memory())
                };

                let (down, up) = {
                    let mut nets = state.nets.lock().unwrap();
                    nets.refresh(true);
                    let rx: u64 = nets.iter().map(|(_, n)| n.received()).sum();
                    let tx: u64 = nets.iter().map(|(_, n)| n.transmitted()).sum();
                    let d = if prev_rx == 0 { 0 } else { rx.saturating_sub(prev_rx) };
                    let u = if prev_tx == 0 { 0 } else { tx.saturating_sub(prev_tx) };
                    prev_rx = rx;
                    prev_tx = tx;
                    (d, u)
                };

                let payload = json!({
                    "cpu": (cpu * 10.0).round() / 10.0,
                    "memUsed": mem_used,
                    "memTotal": mem_total,
                    "up": up,
                    "down": down,
                });
                tick_n += 1;
                if tick_n <= 3 || tick_n % 30 == 0 {
                    println!("[stats] tick {tick_n} cpu={cpu:.1} subs={}", state.subscribers.load(Ordering::SeqCst));
                }
                let _ = app.emit_to("dock", "stats", payload);
            }
        });
    }

    pub fn unsubscribe(&self) {
        // 下限 0：宿主在摘除插件 iframe 时会兜底退订，插件自身 pagehide 也可能
        // 再退一次，重复/超额退订不得把计数打成负数（负数会让下次 subscribe
        // 的 fetch_add 判断失误，采样循环再也无法启动）
        let _ = self.subscribers.fetch_update(Ordering::SeqCst, Ordering::SeqCst,
            |n| Some(if n > 0 { n - 1 } else { 0 }));
    }
}

/// 一键清理内存：遍历进程并 EmptyWorkingSet，把工作集页换出到待机列表。
/// 返回回收过的进程数。安全无副作用（系统按需重新分页），瞬间可能轻微卡顿。
pub fn clean_memory() -> usize {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::ProcessStatus::K32EmptyWorkingSet;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
    };
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap.is_null() {
            return 0;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut count = 0usize;
        let mut ok = Process32FirstW(snap, &mut entry);
        while ok != 0 {
            let h = OpenProcess(PROCESS_SET_QUOTA | PROCESS_QUERY_LIMITED_INFORMATION, 0, entry.th32ProcessID);
            if !h.is_null() {
                // 忽略失败（权限不足的系统进程等）
                if K32EmptyWorkingSet(h) != 0 {
                    count += 1;
                }
                CloseHandle(h);
            }
            ok = Process32NextW(snap, &mut entry);
        }
        windows_sys::Win32::Foundation::CloseHandle(snap);
        count
    }
}
