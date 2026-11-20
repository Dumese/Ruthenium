//! Windows 11「贴靠布局」（Snap Layouts）——自绘标题栏下的最大化按钮补丁。
//!
//! # 为什么需要这个模块
//!
//! `decorations: false` 之后，Windows 只在窗口于 `WM_NCHITTEST` 里回答 `HTMAXBUTTON`
//! 时，才把鼠标悬停在最大化按钮上的行为（贴靠布局浮层、以及点浮层里的分区贴靠）
//! 交给这个窗口。而 WebView2 是铺满客户区的**子窗口**：指针在客户区里时，系统问的是
//! 它，父窗口（Tauri 的窗口）根本收不到那个点的 `WM_NCHITTEST`。所以前端怎么画、
//! 怎么监听都没用（tauri#4531 标 `status: upstream`，tao/winit 也无法从自己的窗口
//! 过程返回 HTMAXBUTTON）。
//!
//! # 做法
//!
//! 在最大化按钮的位置上盖一个**原生子窗口**：它什么都不画（`hbrBackground` 用
//! NULL_BRUSH，并自己吞掉 `WM_ERASEBKGND`），窗口过程无条件回答 `HTMAXBUTTON`。
//! 系统于是认为那里就是最大化按钮，HTML 照样透出来，浮层照常弹出。Electron 的浏览器
//! 框、Windows Terminal 和几个 Tauri 插件（frame / decorum / window-controls /
//! snap-layout）收敛到的都是这一招。
//!
//! # 代价与对策（覆盖层独占那块矩形的鼠标）
//!
//! - webview 再也收不到 hover / press / click → 本模块转发 [`SNAP_HOVER`] /
//!   [`SNAP_PRESS`] 事件，前端用类名复刻 `:hover` / `:active` 观感
//!   （`src/components/chrome/useSnapOverlay.ts`）；
//! - 点击在窗口过程里直接处理（`WM_NCLBUTTONUP` → 父窗口 `WM_SYSCOMMAND`
//!   SC_MAXIMIZE / SC_RESTORE），不再经过 JS 的 `onClick`；
//! - 矩形由前端量 `getBoundingClientRect()` 送进来，不做「右边缘 − 按钮宽 × n」的
//!   算术：按钮宽度、标题栏内边距、响应式断点一变，算术结果就会错位到按钮外面。
//!
//! 只在 Windows 上生效；其它平台同名命令是空实现（前端可以无脑调用）。

#[cfg(target_os = "windows")]
#[tauri::command]
pub fn snap_overlay_rect(
    window: tauri::WebviewWindow,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
) -> Result<(), String> {
    imp::update(window, x, y, width, height)
}

#[cfg(not(target_os = "windows"))]
#[tauri::command]
pub fn snap_overlay_rect(
    _window: tauri::WebviewWindow,
    _x: f64,
    _y: f64,
    _width: f64,
    _height: f64,
) -> Result<(), String> {
    Ok(())
}

#[cfg(target_os = "windows")]
mod imp {
    use std::sync::Mutex;

    use tauri::{AppHandle, Emitter, Manager, WebviewWindow};
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        TrackMouseEvent, TME_LEAVE, TME_NONCLIENT, TRACKMOUSEEVENT,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, GetWindowLongPtrW, IsWindow, IsZoomed, LoadCursorW,
        PostMessageW, RegisterClassW, SetCursor, SetWindowLongPtrW, SetWindowPos, ShowWindow,
        GWLP_USERDATA, HTMAXBUTTON, HWND_TOP, IDC_ARROW, SC_MAXIMIZE, SC_RESTORE, SWP_NOACTIVATE,
        SWP_SHOWWINDOW, SW_HIDE, WM_ERASEBKGND, WM_NCDESTROY, WM_NCHITTEST, WM_NCLBUTTONDOWN,
        WM_NCLBUTTONUP, WM_NCMOUSELEAVE, WM_SETCURSOR, WM_SYSCOMMAND, WNDCLASSW, WS_CHILD,
        WS_CLIPSIBLINGS, WS_VISIBLE,
    };

    use crate::events::{SNAP_HOVER, SNAP_PRESS};

    /// 覆盖层窗口类名（调试/自动化脚本按它找窗口）。
    const CLASS_NAME: &str = "RutheniumSnapOverlay";

    /// 覆盖层状态：随子窗口一起生灭，指针存在 `GWLP_USERDATA`。
    struct Overlay {
        app: AppHandle,
        /// 父窗口（Tauri 主窗口）：最大化/还原要在它身上发 WM_SYSCOMMAND。
        parent: HWND,
        /// 鼠标是否在覆盖层里（WM_NCHITTEST 会持续到来，只在翻转时发事件）。
        hovering: bool,
    }

    impl Overlay {
        fn emit(&self, event: &str, on: bool) {
            if let Err(e) = self.app.emit(event, on) {
                applog!("[snap] emit {event} failed: {e}");
            }
        }
    }

    /// 已创建的子窗口。HWND 是裸指针（不是 Send），放进静态量时降级成 isize。
    static CHILD: Mutex<Option<(isize, isize)>> = Mutex::new(None);

    /// 前端量到的 CSS 像素矩形：宽或高 < 2 视为「没有最大化按钮」（专注模式、
    /// 窗口最小化、布局还没就绪）→ 隐藏覆盖层，避免它悬在别处抢鼠标。
    pub(super) fn update(
        window: WebviewWindow,
        x: f64,
        y: f64,
        width: f64,
        height: f64,
    ) -> Result<(), String> {
        let parent = window.hwnd().map_err(|e| e.to_string())?.0 as isize;
        let app = window.app_handle().clone();
        // SetWindowPos / CreateWindowExW 必须在主线程；裸指针不能跨线程，传地址。
        window
            .run_on_main_thread(move || unsafe {
                apply(parent as HWND, app, x, y, width, height);
            })
            .map_err(|e| e.to_string())
    }

    unsafe fn apply(parent: HWND, app: AppHandle, x: f64, y: f64, width: f64, height: f64) {
        if IsWindow(parent) == 0 {
            return;
        }
        let wanted = width >= 2.0 && height >= 2.0;
        let child = match existing(parent) {
            Some(child) => child,
            None if !wanted => return, // 还没创建过又不需要，别浪费一次窗口创建
            None => {
                let child = create(parent, app);
                if child.is_null() {
                    applog!(
                        "[snap] CreateWindowExW failed: {}",
                        std::io::Error::last_os_error()
                    );
                    return;
                }
                child
            }
        };
        if !wanted {
            ShowWindow(child, SW_HIDE);
            return;
        }
        let dpi = match GetDpiForWindow(parent) {
            0 => 96, // 极端旧系统/窗口尚未关联显示器：按 100% 处理
            dpi => dpi,
        };
        // HWND_TOP + SWP_SHOWWINDOW：覆盖层必须排在 WebView2 子窗口之上，
        // 否则命中测试仍然落在 webview 上（浮层不会出现）。
        SetWindowPos(
            child,
            HWND_TOP,
            to_physical(x, dpi),
            to_physical(y, dpi),
            to_physical(width, dpi).max(1),
            to_physical(height, dpi).max(1),
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
    }

    /// 已存在的覆盖层（校验父窗口一致且窗口仍活）。
    unsafe fn existing(parent: HWND) -> Option<HWND> {
        let guard = CHILD.lock().ok()?;
        let (p, c) = (*guard)?;
        let child = c as HWND;
        if p != parent as isize || IsWindow(child) == 0 {
            return None;
        }
        Some(child)
    }

    unsafe fn create(parent: HWND, app: AppHandle) -> HWND {
        let hinst = GetModuleHandleW(std::ptr::null());
        let class = wide(CLASS_NAME);
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(overlay_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: std::ptr::null_mut(),
            hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
            // NULL_BRUSH：系统永远不替它擦背景 → 整块子窗口一个像素都不画，HTML 透出来。
            // 不要用 WS_EX_LAYERED + alpha 0：那会改变合成行为并让命中测试失效。
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class.as_ptr(),
        };
        // 重复注册只返回 0（ERROR_CLASS_ALREADY_EXISTS），不是失败。
        RegisterClassW(&wc);

        let child = CreateWindowExW(
            0,
            class.as_ptr(),
            std::ptr::null(),
            // WS_CLIPSIBLINGS：与兄弟子窗口（WebView2）互不覆盖绘制；
            // 位置由 SetWindowPos(HWND_TOP) 提到最上层。
            WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS,
            0,
            0,
            1,
            1,
            parent,
            std::ptr::null_mut(),
            hinst,
            std::ptr::null(),
        );
        if child.is_null() {
            return child;
        }
        let state = Box::into_raw(Box::new(Overlay {
            app,
            parent,
            hovering: false,
        }));
        SetWindowLongPtrW(child, GWLP_USERDATA, state as isize);
        if let Ok(mut guard) = CHILD.lock() {
            *guard = Some((parent as isize, child as isize));
        }
        child
    }

    /// 逻辑（CSS）像素 → 物理像素。四舍五入而不是截断：0.5px 的累积漂移会让
    /// 覆盖层比按钮偏一像素，浮层就不在按钮上了。
    fn to_physical(logical: f64, dpi: u32) -> i32 {
        (logical * f64::from(dpi) / 96.0).round() as i32
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    unsafe extern "system" fn overlay_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Overlay;
        match msg {
            // 整个覆盖层就是「最大化按钮」：无条件回答 HTMAXBUTTON，Windows 才会
            // 在悬停时弹贴靠布局浮层。
            WM_NCHITTEST => {
                if let Some(st) = state.as_mut() {
                    if !st.hovering {
                        st.hovering = true;
                        st.emit(SNAP_HOVER, true);
                        // 非客户区也要「鼠标离开」通知，否则 hover 类名会永久残留。
                        let mut tme = TRACKMOUSEEVENT {
                            cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                            dwFlags: TME_LEAVE | TME_NONCLIENT,
                            hwndTrack: hwnd,
                            dwHoverTime: 0,
                        };
                        TrackMouseEvent(&mut tme);
                    }
                    return HTMAXBUTTON as LRESULT;
                }
            }
            WM_NCMOUSELEAVE => {
                if let Some(st) = state.as_mut() {
                    if st.hovering {
                        st.hovering = false;
                        st.emit(SNAP_HOVER, false);
                    }
                }
            }
            WM_NCLBUTTONDOWN => {
                if wparam == HTMAXBUTTON as usize {
                    if let Some(st) = state.as_mut() {
                        st.emit(SNAP_PRESS, true);
                    }
                    // 吞掉：不让 DefWindowProc 去走它自己的标题按钮跟踪。
                    return 0;
                }
            }
            WM_NCLBUTTONUP => {
                if wparam == HTMAXBUTTON as usize {
                    if let Some(st) = state.as_mut() {
                        st.emit(SNAP_PRESS, false);
                        let cmd = if IsZoomed(st.parent) != 0 {
                            SC_RESTORE
                        } else {
                            SC_MAXIMIZE
                        };
                        // Post 而不是 Send：不在这里重入父窗口的窗口过程。
                        PostMessageW(st.parent, WM_SYSCOMMAND, cmd as WPARAM, 0);
                    }
                    return 0;
                }
            }
            // 不擦背景 → 完全不绘制（NULL_BRUSH 的兜底）。
            WM_ERASEBKGND => return 1,
            WM_SETCURSOR => {
                SetCursor(LoadCursorW(std::ptr::null_mut(), IDC_ARROW));
                return 1;
            }
            WM_NCDESTROY => {
                if !state.is_null() {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    drop(Box::from_raw(state));
                }
                if let Ok(mut guard) = CHILD.lock() {
                    if matches!(*guard, Some((_, c)) if c == hwnd as isize) {
                        *guard = None;
                    }
                }
                return DefWindowProcW(hwnd, msg, wparam, lparam);
            }
            _ => {}
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }

    #[cfg(test)]
    mod tests {
        use super::to_physical;

        /// 100% 缩放：CSS 像素即物理像素。
        #[test]
        fn physical_at_96_dpi_is_identity() {
            assert_eq!(to_physical(0.0, 96), 0);
            assert_eq!(to_physical(46.0, 96), 46);
            assert_eq!(to_physical(40.0, 96), 40);
        }

        /// 125% / 150% / 200%：按 dpi/96 放缩并四舍五入。
        #[test]
        fn physical_scales_with_dpi() {
            assert_eq!(to_physical(46.0, 120), 58); // 57.5 → 58
            assert_eq!(to_physical(46.0, 144), 69);
            assert_eq!(to_physical(40.0, 144), 60);
            assert_eq!(to_physical(46.0, 192), 92);
            // 小数 CSS 矩形（getBoundingClientRect 会给出 .5）：不能截断成 57
            assert_eq!(to_physical(45.5, 120), 57);
            assert_eq!(to_physical(45.9, 120), 57);
        }
    }
}
