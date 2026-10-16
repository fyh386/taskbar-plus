//! 任务栏锚定：检测主任务栏（Shell_TrayWnd）与托盘区（TrayNotifyWnd）位置，
//! 将 dock 窗口吸附在任务栏空白区域（托盘左侧），每 2s 跟随一次变化。

use tauri::Manager;
use windows_sys::Win32::Foundation::{HWND, RECT};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    FindWindowExW, FindWindowW, GetSystemMetrics, GetWindowRect, SetWindowPos, HWND_TOPMOST,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
};

#[derive(Debug, Clone, Copy, PartialEq)]
enum Edge {
    Bottom,
    Top,
    Left,
    Right,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe fn find_window(class: &str) -> HWND {
    FindWindowW(wide(class).as_ptr(), std::ptr::null())
}

unsafe fn find_child(parent: HWND, class: &str) -> HWND {
    FindWindowExW(parent, std::ptr::null_mut(), wide(class).as_ptr(), std::ptr::null())
}

unsafe fn window_rect(hwnd: HWND) -> Option<RECT> {
    if hwnd.is_null() {
        return None;
    }
    let mut r: RECT = std::mem::zeroed();
    if GetWindowRect(hwnd, &mut r) == 0 {
        return None;
    }
    Some(r)
}

/// Compute dock position/size; None if taskbar not found.
fn compute_dock_rect(width: u32, extra: u32) -> Option<(i32, i32, u32, u32)> {
    unsafe {
        let taskbar = find_window("Shell_TrayWnd");
        let tb = window_rect(taskbar)?;
        let screen_w = GetSystemMetrics(windows_sys::Win32::UI::WindowsAndMessaging::SM_CXSCREEN);

        let tb_w = tb.right - tb.left;
        let tb_h = tb.bottom - tb.top;
        let edge = if tb.top == 0 && tb_h > tb_w {
            Edge::Left
        } else if tb.left == 0 && tb_w > tb_h && tb.top <= 2 && tb_h < 200 {
            Edge::Top
        } else if tb.left > screen_w / 2 {
            Edge::Right
        } else {
            Edge::Bottom
        };

        match edge {
            Edge::Bottom => {
                // tray width
                let tray = find_child(taskbar, "TrayNotifyWnd");
                let tray_left = window_rect(tray).map(|r| r.left).unwrap_or(tb.right);
                // 弹层向上展开，高度不超过任务栏顶部（弹层顶不超出屏幕上缘，否则上边界被裁）
                let extra = extra.min(tb.top.max(0) as u32);
                let h = tb_h.max(40) as u32 + extra;
                let max_w = (tray_left - tb.left - 16).max(80) as u32;
                let w = width.min(max_w);
                let x = tray_left - w as i32 - 8;
                Some((x, tb.top - extra as i32, w, h))
            }
            Edge::Top => {
                let tray = find_child(taskbar, "TrayNotifyWnd");
                let tray_left = window_rect(tray).map(|r| r.left).unwrap_or(tb.right);
                let h = tb_h.max(40) as u32 + extra;
                let max_w = (tray_left - tb.left - 16).max(80) as u32;
                let w = width.min(max_w);
                Some((tray_left - w as i32 - 8, tb.top, w, h))
            }
            // vertical taskbar (left/right): dock near top (simplified)
            Edge::Left | Edge::Right => {
                let h = 160u32 + extra;
                let w = tb_w.max(40) as u32;
                let x = tb.left;
                let y = tb.top + 8;
                Some((x, y, w, h))
            }
        }
    }
}

pub fn apply_anchor(app: &tauri::AppHandle, width: u32) {
    let Some(win) = app.get_webview_window("dock") else { return };
    let extra = {
        let state = app.state::<crate::AppState>();
        state.dock_extra.load(std::sync::atomic::Ordering::Relaxed)
    };
    if let Some((x, y, w, h)) = compute_dock_rect(width, extra) {
        // 水平位置平滑：托盘区宽度会因图标增减/通知闪烁而抖动，直接跟随会让
        // 整条 dock 左右滑动（用户感知为"任务栏抖动"）。目标位置需连续 2 次
        // 检测一致（约 4s）或变化剧透(>60px)才应用；y/w/h 立即跟随（弹层开合不滞后）。
        let (x, w) = smooth_xw(x, w);
        // 差异更新：几何未变时不做任何 Win32 调用
        let cur_pos = win.outer_position().map(|p| (p.x, p.y)).unwrap_or((i32::MAX, i32::MAX));
        let cur_size = win.outer_size().map(|s| (s.width, s.height)).unwrap_or((0, 0));
        let moved = (cur_pos.0 - x).abs() > 2 || (cur_pos.1 - y).abs() > 2;
        let resized = (cur_size.0 as i32 - w as i32).abs() > 2 || (cur_size.1 as i32 - h as i32).abs() > 2;
        if moved || resized {
            if let Ok(hwnd) = win.hwnd() {
                unsafe {
                    SetWindowPos(
                        hwnd.0 as HWND,
                        std::ptr::null_mut(), // SWP_NOZORDER：保持现有层级（置顶属性不变）
                        x,
                        y,
                        w as i32,
                        h as i32,
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                }
            }
        }
        let _ = win.show();
    }
}

struct SmoothState {
    applied_x: i32,
    applied_w: u32,
    pending_x: i32,
    pending_n: u32,
}

fn smooth_xw(x: i32, w: u32) -> (i32, u32) {
    use std::sync::Mutex;
    static S: Mutex<Option<SmoothState>> = Mutex::new(None);
    let mut s = S.lock().unwrap();
    let st = s.get_or_insert_with(|| SmoothState { applied_x: x, applied_w: w, pending_x: x, pending_n: 0 });
    if (x - st.pending_x).abs() <= 2 {
        st.pending_n += 1;
    } else {
        st.pending_x = x;
        st.pending_n = 0;
    }
    if st.pending_n >= 2 || (x - st.applied_x).abs() > 60 {
        // 位置稳定或剧变：应用新位置（x 与 w 联动，右边缘始终对齐托盘左侧）
        st.applied_x = x;
        st.applied_w = w;
        (x, w)
    } else {
        // 托盘宽度瞬态抖动：x/w 都保持已应用值，dock 纹丝不动
        (st.applied_x, st.applied_w)
    }
}

/// Background loop: taskbar position/size/tray width may change over time.
pub fn start_anchor_loop(app: tauri::AppHandle) {
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let width = {
                let state = app.state::<crate::AppState>();
                state.dock_width.load(std::sync::atomic::Ordering::Relaxed)
            };
            apply_anchor(&app, width);
            reassert_topmost(&app);
        }
    });
}

/// 周期性重申置顶：第三方软件（如 360 桌面围栏）会把自身画布窗口反复提到
/// 最顶层带，把 dock 压到任务栏/桌面之下导致整条消失。每 2s 把 dock 提回
/// 顶层带顶部即可自愈；同层级调用开销极小，不与正常窗口交互。
fn reassert_topmost(app: &tauri::AppHandle) {
    let Some(win) = app.get_webview_window("dock") else { return };
    if let Ok(hwnd) = win.hwnd() {
        unsafe {
            SetWindowPos(
                hwnd.0 as HWND,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE,
            );
        }
    }
}

/// （备用）整窗玻璃化：margins=-1 让客户区成为扩展帧，可去除 DWM 阴影。
/// 注意：对 WebView2 透明窗口会导致内容不合成（窗口整体不可见），当前不启用。
#[allow(dead_code)]
pub fn make_glass(win: &tauri::WebviewWindow) {
    use windows_sys::Win32::Graphics::Dwm::DwmExtendFrameIntoClientArea;
    use windows_sys::Win32::UI::Controls::MARGINS;
    if let Ok(hwnd) = win.hwnd() {
        let m = MARGINS {
            cxLeftWidth: -1,
            cyTopHeight: -1,
            cxRightWidth: -1,
            cyBottomHeight: -1,
        };
        unsafe {
            DwmExtendFrameIntoClientArea(hwnd.0, &m);
        }
    }
}
