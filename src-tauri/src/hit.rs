//! 弹层外部鼠标穿透：dock 窗口在插件展开时被加高（弹层在条带上方），窗口矩形
//! 内弹层之外的多余区域（空白带、向左透明延伸区）对鼠标透明——WM_NCHITTEST
//! 子类返回 HTTRANSPARENT，点击落到后方窗口；条带行、弹层本体、右键菜单保持可交互。
//! 可点击区域由前端 layout() 以窗口客户区坐标（CSS px）上报，按窗口 DPI 换算。
//!
//! 注意：SetWindowRgn 方案已实测否决——它会让 WebView2 的 DComp 合成把
//! 弹层错误裁剪掉（弹层消失），只能走命中测试子类这一路。
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::RwLock;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::ScreenToClient;
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};

const WM_SETCURSOR: u32 = 0x0020;
const WM_MOUSEACTIVATE: u32 = 0x0021;
const WM_ACTIVATE: u32 = 0x0006;
const WM_NCHITTEST: u32 = 0x0084;
const WM_MOUSEMOVE: u32 = 0x0200;
const WM_LBUTTONDOWN: u32 = 0x0201;
const WM_LBUTTONUP: u32 = 0x0202;
const HTTRANSPARENT: LRESULT = -1;

/// 诊断：真实输入到底给 dock 窗口发了哪些消息（每类只记前 3 次）
const WATCH: &[u32] = &[
    WM_NCHITTEST,
    WM_SETCURSOR,
    WM_MOUSEMOVE,
    WM_LBUTTONDOWN,
    WM_LBUTTONUP,
    WM_ACTIVATE,
    WM_MOUSEACTIVATE,
];
const ZERO: AtomicU32 = AtomicU32::new(0);
static WATCH_C: [AtomicU32; 7] = [ZERO; 7];

#[derive(Debug, Clone)]
pub struct HitRegions {
    /// 物理 px / CSS px（按 dock 窗口 DPI）
    pub scale: f64,
    /// 可点击矩形，窗口客户区坐标，CSS px：[x, y, w, h]
    pub rects: Vec<[i32; 4]>,
}

static REGIONS: RwLock<Option<HitRegions>> = RwLock::new(None);
static INSTALLED: AtomicBool = AtomicBool::new(false);

pub fn set_regions(regions: Option<HitRegions>) {
    if let Ok(mut g) = REGIONS.write() {
        *g = regions;
    }
}

/// 在 dock 窗口上安装子类（幂等）。命中测试未配置（None）时全部走默认处理。
pub fn install(hwnd: HWND) {
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    let ok = unsafe { SetWindowSubclass(hwnd, Some(hit_subclass), 0x5453_5001, 0) };
    crate::flog("[hit] ", &format!("subclass install ok={ok} hwnd={hwnd:p}"));
}

pub fn scale_for(hwnd: HWND) -> f64 {
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    if dpi == 0 { 1.0 } else { dpi as f64 / 96.0 }
}

fn diag_watch(msg: u32, lparam: LPARAM) {
    for (i, m) in WATCH.iter().enumerate() {
        if *m == msg {
            let n = WATCH_C[i].fetch_add(1, Ordering::Relaxed);
            if n < 3 {
                crate::flog(
                    "[hit] ",
                    &format!("msg=0x{msg:04x} count={n} lparam={lparam:#x}"),
                );
            }
            return;
        }
    }
}

unsafe extern "system" fn hit_subclass(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _uid: usize,
    _ref: usize,
) -> LRESULT {
    diag_watch(msg, lparam);
    if msg == WM_NCHITTEST {
        if let Ok(g) = REGIONS.read() {
            if let Some(reg) = g.as_ref() {
                if !reg.rects.is_empty() {
                    // lparam：屏幕坐标（有符号 16 位）
                    let x = (lparam as i32) & 0xffff;
                    let y = ((lparam as i32) >> 16) & 0xffff;
                    let mut pt = POINT {
                        x: (x as i16) as i32,
                        y: (y as i16) as i32,
                    };
                    if ScreenToClient(hwnd, &mut pt) != 0 {
                        let s = reg.scale;
                        for [rx, ry, rw, rh] in &reg.rects {
                            let l = (*rx as f64 * s) as i32;
                            let t = (*ry as f64 * s) as i32;
                            let r = ((*rx + *rw) as f64 * s) as i32;
                            let b = ((*ry + *rh) as f64 * s) as i32;
                            if pt.x >= l && pt.x < r && pt.y >= t && pt.y < b {
                                return DefSubclassProc(hwnd, msg, wparam, lparam);
                            }
                        }
                        // 不在任何可点击区域：穿透到后方窗口
                        return HTTRANSPARENT;
                    }
                }
            }
        }
    }
    DefSubclassProc(hwnd, msg, wparam, lparam)
}
