//! 极简启动界面：选一个窗口 + 几个可选配置（模型、模型输入尺寸、上屏尺寸）。
//!
//! 纯 Win32（LISTBOX/EDIT/BUTTON），不引额外 GUI 依赖。
//! 关掉窗口 = 取消（返回 `None`），主程序随即退出。

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard};
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    ERROR_CLASS_ALREADY_EXISTS, GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{COLOR_BTNFACE, GetSysColorBrush, UpdateWindow};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::Dialogs::{
    GetOpenFileNameW, OFN_FILEMUSTEXIST, OFN_PATHMUSTEXIST, OPENFILENAMEW,
};
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::*;

const ID_LIST: i32 = 1001;
const ID_OK: i32 = 1002;
const ID_MODEL: i32 = 1003;
const ID_BROWSE: i32 = 1004;
const ID_IN_W: i32 = 1005;
const ID_IN_H: i32 = 1006;
const ID_OUT_W: i32 = 1007;
const ID_OUT_H: i32 = 1008;

/// 一次运行需要的全部参数
#[derive(Clone, Debug)]
pub struct Picked {
    pub hwnd: HWND,
    pub title: String,
    /// 模型文件路径
    pub model_path: String,
    /// 模型输入宽高；`None` = 与所选窗口客户区一致
    pub model_input: Option<(u32, u32)>,
    /// 上屏窗口宽高；`None` = 与所选窗口客户区一致
    pub output_size: Option<(u32, u32)>,
}

/// 界面上各输入框的默认值
#[derive(Clone, Debug)]
pub struct Defaults {
    pub model_path: String,
    pub model_input: Option<(u32, u32)>,
    pub output_size: Option<(u32, u32)>,
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            model_path: "AnimeGANv3_Hayao_36.onnx".to_string(),
            model_input: None,
            output_size: None,
        }
    }
}

/// 用户点「开始渲染」那一刻的全部参数；`None` = 关掉界面取消
struct Confirmed {
    hwnd: isize,
    title: String,
    model_path: String,
    model_input: Option<(u32, u32)>,
    output_size: Option<(u32, u32)>,
}

static CONFIRMED: Mutex<Option<Confirmed>> = Mutex::new(None);

/// 界面窗口已销毁（点 X 或点了「开始渲染」）。退出消息循环只认它，不认 WM_QUIT
static CLOSED: AtomicBool = AtomicBool::new(false);

/// 创建界面时用的 DPI。窗口尺寸和控件坐标都按它算，保证控件一定落在客户区里
static PICKER_DPI: AtomicU32 = AtomicU32::new(96);

fn confirmed_slot() -> MutexGuard<'static, Option<Confirmed>> {
    CONFIRMED.lock().unwrap_or_else(|e| e.into_inner())
}

/// 取窗口标题（日志用）
fn window_title_of(hwnd: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return String::new();
        }
        let mut buf = vec![0u16; len as usize + 1];
        let n = GetWindowTextW(hwnd, &mut buf);
        String::from_utf16_lossy(&buf[..n.max(0) as usize])
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

struct Candidate {
    hwnd: HWND,
    label: String,
    /// 客户区面积：列表按它从大到小排，游戏窗口通常最靠前，默认选中第一个即可
    area: i64,
}

extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
    unsafe {
        let list = &mut *(lparam.0 as *mut Vec<Candidate>);
        if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
            return true.into();
        }
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return true.into();
        }
        let mut buf = vec![0u16; len as usize + 1];
        let n = GetWindowTextW(hwnd, &mut buf);
        let title = String::from_utf16_lossy(&buf[..n.max(0) as usize]);
        if title.trim().is_empty() {
            return true.into();
        }
        // 跳过自己（上屏窗口 / 这个选择界面都算自己）
        let mut pid = 0u32;
        let _ = GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == std::process::id() {
            return true.into();
        }
        // 跳过工具窗（各种悬浮层）和 UWP 的隐藏窗口
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        if ex & WS_EX_TOOLWINDOW.0 != 0 || is_cloaked(hwnd) {
            return true.into();
        }
        let mut cls = [0u16; 64];
        let cl = GetClassNameW(hwnd, &mut cls);
        let cls = String::from_utf16_lossy(&cls[..cl.max(0) as usize]);
        if matches!(
            cls.as_str(),
            "Progman" | "WorkerW" | "Shell_TrayWnd" | "Windows.UI.Core.CoreWindow"
        ) {
            return true.into();
        }
        let mut client = RECT::default();
        if GetClientRect(hwnd, &mut client).is_err() {
            return true.into();
        }
        let (w, h) = (client.right - client.left, client.bottom - client.top);
        if w < 64 || h < 64 {
            return true.into();
        }
        list.push(Candidate {
            hwnd,
            label: format!("{title}    [{w}x{h}]"),
            area: w as i64 * h as i64,
        });
    }
    true.into()
}

/// UWP 应用不在前台时窗口是「cloaked」状态，看不见但 IsWindowVisible 为真
fn is_cloaked(hwnd: HWND) -> bool {
    use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
    unsafe {
        let mut cloaked = 0u32;
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut _,
            std::mem::size_of::<u32>() as u32,
        )
        .is_ok()
            && cloaked != 0
    }
}

fn candidates() -> Vec<Candidate> {
    let mut list: Vec<Candidate> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(enum_proc), LPARAM(&mut list as *mut Vec<Candidate> as isize));
    }
    // 大的窗口排在前面（游戏一般最大，默认选中的第一个就是它）
    list.sort_by(|a, b| {
        b.area
            .cmp(&a.area)
            .then_with(|| a.label.to_lowercase().cmp(&b.label.to_lowercase()))
    });
    list
}

/// 取子控件文字
unsafe fn text_of(parent: HWND, id: i32) -> String {
    unsafe {
        let Ok(ctrl) = GetDlgItem(Some(parent), id) else {
            return String::new();
        };
        let len = GetWindowTextLengthW(ctrl);
        if len <= 0 {
            return String::new();
        }
        let mut buf = vec![0u16; len as usize + 1];
        let n = GetWindowTextW(ctrl, &mut buf);
        String::from_utf16_lossy(&buf[..n.max(0) as usize])
    }
}

/// 解析留空的宽高输入：两个都填了才算有效
fn parse_size(w: &str, h: &str) -> Result<Option<(u32, u32)>, String> {
    let (w, h) = (w.trim(), h.trim());
    if w.is_empty() && h.is_empty() {
        return Ok(None);
    }
    let w: u32 = w.parse().map_err(|_| format!("宽度不是数字: {w}"))?;
    let h: u32 = h.parse().map_err(|_| format!("高度不是数字: {h}"))?;
    if !(16..=16384).contains(&w) || !(16..=16384).contains(&h) {
        return Err(format!("尺寸超出范围: {w}x{h}"));
    }
    Ok(Some((w, h)))
}

/// 确认：校验输入，记录结果并关闭界面
unsafe fn accept(hwnd: HWND) {
    unsafe {
        /// 校验失败：界面上给一行提示，日志里也留一条，方便事后查
        unsafe fn reject(hwnd: HWND, msg: &str) {
            unsafe { set_status(hwnd, msg) };
            log::warn!("选择界面：{msg}");
        }
        let Ok(list) = GetDlgItem(Some(hwnd), ID_LIST) else {
            log::warn!("选择界面：找不到窗口列表控件");
            return;
        };
        let sel = SendMessageW(list, LB_GETCURSEL, None, None).0;
        if sel < 0 {
            reject(hwnd, "请先在上面的列表里选一个窗口");
            return;
        }
        let target = SendMessageW(list, LB_GETITEMDATA, Some(WPARAM(sel as usize)), None).0;
        if target == 0 {
            reject(hwnd, "这一项没有窗口句柄，换一个");
            return;
        }
        let model = text_of(hwnd, ID_MODEL).trim().to_string();
        if model.is_empty() {
            reject(hwnd, "请填写模型文件路径");
            return;
        }
        if !std::path::Path::new(&model).is_file() {
            // 相对路径是相对「当前工作目录」解析的，顺便把它打出来
            let cwd = std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            reject(hwnd, &format!("找不到模型文件：{model}"));
            log::warn!("选择界面：当前工作目录 = {cwd}");
            return;
        }
        let model_input = match parse_size(&text_of(hwnd, ID_IN_W), &text_of(hwnd, ID_IN_H)) {
            Ok(v) => v,
            Err(e) => {
                reject(hwnd, &format!("模型输入宽高有误：{e}"));
                return;
            }
        };
        let output_size = match parse_size(&text_of(hwnd, ID_OUT_W), &text_of(hwnd, ID_OUT_H)) {
            Ok(v) => v,
            Err(e) => {
                reject(hwnd, &format!("上屏宽高有误：{e}"));
                return;
            }
        };
        let target = HWND(target as *mut core::ffi::c_void);
        let title = window_title_of(target);
        log::info!(
            "选择界面：确认 hwnd={:?}「{title}」模型={model} 输入={model_input:?} 上屏={output_size:?}",
            target
        );
        *confirmed_slot() = Some(Confirmed {
            hwnd: target.0 as isize,
            title,
            model_path: model,
            model_input,
            output_size,
        });
        let _ = DestroyWindow(hwnd);
    }
}

unsafe fn set_status(hwnd: HWND, text: &str) {
    unsafe {
        if let Ok(ctrl) = GetDlgItem(Some(hwnd), ID_OK + 100) {
            let w = wide(text);
            let _ = SetWindowTextW(ctrl, PCWSTR::from_raw(w.as_ptr()));
        }
    }
}

/// 模型文件选择框
unsafe fn browse_model(hwnd: HWND) {
    unsafe {
        let mut buf = [0u16; 1024];
        let cur = text_of(hwnd, ID_MODEL);
        for (i, c) in cur.encode_utf16().take(1000).enumerate() {
            buf[i] = c;
        }
        let filter: Vec<u16> = "ONNX 模型 (*.onnx)\0*.onnx\0所有文件 (*.*)\0*.*\0\0"
            .encode_utf16()
            .collect();
        let mut ofn = OPENFILENAMEW {
            lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
            // 指定 owner：界面是置顶的，不指定的话文件对话框会被压在界面后面
            hwndOwner: hwnd,
            lpstrFile: PWSTR(buf.as_mut_ptr()),
            nMaxFile: buf.len() as u32,
            lpstrFilter: PCWSTR::from_raw(filter.as_ptr()),
            Flags: OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST,
            ..Default::default()
        };
        if GetOpenFileNameW(&mut ofn).as_bool() {
            if let Ok(ctrl) = GetDlgItem(Some(hwnd), ID_MODEL) {
                let end = buf.iter().position(|&c| c == 0).unwrap_or(0);
                let text = wide(&String::from_utf16_lossy(&buf[..end]));
                let _ = SetWindowTextW(ctrl, PCWSTR::from_raw(text.as_ptr()));
            }
        }
    }
}

fn statics(scale: f32, v: i32) -> i32 {
    (v as f32 * scale) as i32
}

/// 创建一个控件（宽固定，返回值忽略错误）
unsafe fn make_child(
    parent: HWND,
    class: &str,
    text: &str,
    style: u32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    id: i32,
) {
    unsafe {
        let instance = GetModuleHandleW(None).unwrap_or_default();
        let cls = wide(class);
        let txt = wide(text);
        let _ = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR::from_raw(cls.as_ptr()),
            PCWSTR::from_raw(txt.as_ptr()),
            WINDOW_STYLE(style),
            x,
            y,
            w,
            h,
            Some(parent),
            Some(HMENU(id as isize as *mut core::ffi::c_void)),
            Some(HINSTANCE(instance.0)),
            None,
        );
    }
}

extern "system" fn picker_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_CREATE => {
                // 用创建窗口时记录的 DPI（不是 GetDpiForWindow：WM_CREATE 时窗口
                // 往往还没关联到显示器，返回值可能是 0，控件会按错的缩放摆放）
                let scale = PICKER_DPI.load(Ordering::SeqCst).max(96) as f32 / 96.0;
                let s = |v: i32| statics(scale, v);
                // WM_CREATE 的 lparam 是 CREATESTRUCTW 指针，真正的参数在 lpCreateParams 里
                let cs = &*(lparam.0 as *const CREATESTRUCTW);
                let defaults = &*(cs.lpCreateParams as *const Defaults);

                // ① 窗口列表
                make_child(
                    hwnd,
                    "STATIC",
                    "① 选择要渲染的窗口",
                    (WS_CHILD | WS_VISIBLE).0,
                    s(12),
                    s(8),
                    s(660),
                    s(18),
                    -1,
                );
                let list = CreateWindowExW(
                    WS_EX_CLIENTEDGE,
                    PCWSTR::from_raw(wide("LISTBOX").as_ptr()),
                    PCWSTR::null(),
                    WS_CHILD
                        | WS_VISIBLE
                        | WS_VSCROLL
                        | WINDOW_STYLE((LBS_NOTIFY | LBS_HASSTRINGS | LBS_NOINTEGRALHEIGHT) as u32),
                    s(12),
                    s(28),
                    s(700),
                    s(300),
                    Some(hwnd),
                    Some(HMENU(ID_LIST as isize as *mut core::ffi::c_void)),
                    Some(HINSTANCE(GetModuleHandleW(None).unwrap_or_default().0)),
                    None,
                );
                if let Ok(list) = list {
                    for c in candidates() {
                        let label = wide(&c.label);
                        let idx = SendMessageW(
                            list,
                            LB_ADDSTRING,
                            None,
                            Some(LPARAM(label.as_ptr() as isize)),
                        )
                        .0;
                        if idx >= 0 {
                            SendMessageW(
                                list,
                                LB_SETITEMDATA,
                                Some(WPARAM(idx as usize)),
                                Some(LPARAM(c.hwnd.0 as isize)),
                            );
                        }
                    }
                    SendMessageW(list, LB_SETCURSEL, Some(WPARAM(0)), None);
                    let _ = windows::Win32::UI::Input::KeyboardAndMouse::SetFocus(Some(list));
                }

                // ② 模型文件
                make_child(
                    hwnd,
                    "STATIC",
                    "② 模型文件 (.onnx)",
                    (WS_CHILD | WS_VISIBLE).0,
                    s(12),
                    s(342),
                    s(160),
                    s(18),
                    -1,
                );
                make_child(
                    hwnd,
                    "EDIT",
                    &defaults.model_path,
                    (WS_CHILD | WS_VISIBLE | WS_BORDER | WS_TABSTOP | WINDOW_STYLE(ES_AUTOHSCROLL as u32))
                        .0,
                    s(180),
                    s(338),
                    s(430),
                    s(24),
                    ID_MODEL,
                );
                make_child(
                    hwnd,
                    "BUTTON",
                    "浏览...",
                    (WS_CHILD | WS_VISIBLE | WS_TABSTOP).0,
                    s(618),
                    s(337),
                    s(94),
                    s(26),
                    ID_BROWSE,
                );

                // ③ 模型输入宽高（留空 = 自动）
                make_child(
                    hwnd,
                    "STATIC",
                    "③ 模型输入宽高（留空 = 与所选窗口一致）",
                    (WS_CHILD | WS_VISIBLE).0,
                    s(12),
                    s(376),
                    s(300),
                    s(18),
                    -1,
                );
                let (in_w, in_h) = defaults
                    .model_input
                    .map(|(w, h)| (w.to_string(), h.to_string()))
                    .unwrap_or_default();
                make_child(
                    hwnd,
                    "EDIT",
                    &in_w,
                    (WS_CHILD | WS_VISIBLE | WS_BORDER | WS_TABSTOP | WINDOW_STYLE(ES_NUMBER as u32)).0,
                    s(320),
                    s(372),
                    s(70),
                    s(24),
                    ID_IN_W,
                );
                make_child(
                    hwnd,
                    "STATIC",
                    "x",
                    (WS_CHILD | WS_VISIBLE).0,
                    s(396),
                    s(376),
                    s(14),
                    s(18),
                    -1,
                );
                make_child(
                    hwnd,
                    "EDIT",
                    &in_h,
                    (WS_CHILD | WS_VISIBLE | WS_BORDER | WS_TABSTOP | WINDOW_STYLE(ES_NUMBER as u32)).0,
                    s(414),
                    s(372),
                    s(70),
                    s(24),
                    ID_IN_H,
                );

                // ④ 上屏宽高（留空 = 与窗口一致）
                make_child(
                    hwnd,
                    "STATIC",
                    "④ 上屏宽高（留空 = 与所选窗口一致）",
                    (WS_CHILD | WS_VISIBLE).0,
                    s(12),
                    s(410),
                    s(300),
                    s(18),
                    -1,
                );
                let (out_w, out_h) = defaults
                    .output_size
                    .map(|(w, h)| (w.to_string(), h.to_string()))
                    .unwrap_or_default();
                make_child(
                    hwnd,
                    "EDIT",
                    &out_w,
                    (WS_CHILD | WS_VISIBLE | WS_BORDER | WS_TABSTOP | WINDOW_STYLE(ES_NUMBER as u32)).0,
                    s(320),
                    s(406),
                    s(70),
                    s(24),
                    ID_OUT_W,
                );
                make_child(
                    hwnd,
                    "STATIC",
                    "x",
                    (WS_CHILD | WS_VISIBLE).0,
                    s(396),
                    s(410),
                    s(14),
                    s(18),
                    -1,
                );
                make_child(
                    hwnd,
                    "EDIT",
                    &out_h,
                    (WS_CHILD | WS_VISIBLE | WS_BORDER | WS_TABSTOP | WINDOW_STYLE(ES_NUMBER as u32)).0,
                    s(414),
                    s(406),
                    s(70),
                    s(24),
                    ID_OUT_H,
                );

                // ⑤ 状态行（上一次的错误会显示在这里）
                make_child(
                    hwnd,
                    "STATIC",
                    "",
                    (WS_CHILD | WS_VISIBLE).0,
                    s(12),
                    s(444),
                    s(560),
                    s(36),
                    ID_OK + 100,
                );

                make_child(
                    hwnd,
                    "BUTTON",
                    "开始渲染",
                    (WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_DEFPUSHBUTTON as u32)).0,
                    s(596),
                    s(444),
                    s(116),
                    s(34),
                    ID_OK,
                );
                LRESULT(0)
            }
            WM_COMMAND => {
                let id = (wparam.0 & 0xFFFF) as i32;
                log::info!("选择界面：WM_COMMAND id={id}");
                // 只认按钮点击：列表框在自动选中首项时也会发通知，别被它误触发
                if id == ID_OK {
                    accept(hwnd);
                } else if id == ID_BROWSE {
                    browse_model(hwnd);
                } else if id == IDCANCEL.0 as i32 {
                    // Esc：关掉界面（等于取消），不至于卡在这里只能去任务栏退出
                    let _ = DestroyWindow(hwnd);
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                // 必须真的销毁窗口：只发 WM_QUIT 的话界面会留在屏幕上不消失
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                CLOSED.store(true, Ordering::SeqCst);
                LRESULT(0)
            }
            WM_ACTIVATE => {
                log::info!("选择界面：WM_ACTIVATE（是否拿到焦点）");
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

/// 客户区精确补正：`AdjustWindowRectEx` 在高 DPI 下估的边框不一定准，
/// 客户区小一圈就会把最下面的「开始渲染」按钮挤到窗口外面（看起来像界面没反应）。
unsafe fn fit_client_size(hwnd: HWND, client_w: i32, client_h: i32) {
    unsafe {
        for _ in 0..3 {
            let (mut outer, mut client) = (RECT::default(), RECT::default());
            if GetWindowRect(hwnd, &mut outer).is_err() || GetClientRect(hwnd, &mut client).is_err() {
                return;
            }
            let cur_w = client.right - client.left;
            let cur_h = client.bottom - client.top;
            if cur_w == client_w && cur_h == client_h {
                return;
            }
            let dw = (outer.right - outer.left) - cur_w;
            let dh = (outer.bottom - outer.top) - cur_h;
            let _ = SetWindowPos(
                hwnd,
                None,
                0,
                0,
                client_w + dw,
                client_h + dh,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }
}

/// 摆到显示器工作区正中：不依赖系统的默认级联位置，保证界面（含底部按钮）都在屏幕内
unsafe fn center_on_work_area(hwnd: HWND) {
    unsafe {
        use windows::Win32::Graphics::Gdi::{
            GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
        };
        let mut outer = RECT::default();
        if GetWindowRect(hwnd, &mut outer).is_err() {
            return;
        }
        let (w, h) = (outer.right - outer.left, outer.bottom - outer.top);
        let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(mon, &mut mi).as_bool() {
            return;
        }
        let work = mi.rcWork;
        let x = work.left + ((work.right - work.left) - w) / 2;
        let y = work.top + ((work.bottom - work.top) - h) / 2;
        let _ = SetWindowPos(
            hwnd,
            None,
            x,
            y,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

/// 弹出启动界面；用户取消时返回 `None`。
pub fn pick(defaults: &Defaults) -> Result<Option<Picked>, String> {
    *confirmed_slot() = None;
    CLOSED.store(false, Ordering::SeqCst);
    unsafe {
        let instance = GetModuleHandleW(None).map_err(|e| format!("GetModuleHandleW: {e}"))?;
        let class = wide("AnimationImageeWindowPicker");
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(picker_proc),
            hInstance: HINSTANCE(instance.0),
            lpszClassName: PCWSTR::from_raw(class.as_ptr()),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: GetSysColorBrush(COLOR_BTNFACE),
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS
        {
            return Err("RegisterClassW 失败".into());
        }

        let dpi = GetDpiForSystem().max(96);
        PICKER_DPI.store(dpi, Ordering::SeqCst);
        let scale = dpi as f32 / 96.0;
        let client_w = (726.0 * scale) as i32;
        let client_h = (490.0 * scale) as i32;
        let style = WS_OVERLAPPEDWINDOW & !WS_THICKFRAME & !WS_MAXIMIZEBOX;
        let mut outer = RECT {
            left: 0,
            top: 0,
            right: client_w,
            bottom: client_h,
        };
        let _ = AdjustWindowRectEx(&mut outer, style, false, WINDOW_EX_STYLE(0));

        let title = wide("AnimeGAN - 选择窗口");
        // 把 defaults 指针通过 CREATESTRUCT 传进 WM_CREATE
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR::from_raw(class.as_ptr()),
            PCWSTR::from_raw(title.as_ptr()),
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            outer.right - outer.left,
            outer.bottom - outer.top,
            None,
            None,
            Some(HINSTANCE(instance.0)),
            Some(defaults as *const Defaults as *const core::ffi::c_void),
        )
        .map_err(|e| format!("CreateWindowExW: {e}"))?;

        // 上屏窗口刚关掉时前台通常是游戏：只 ShowWindow 不会把界面抬上来，
        // 界面会被压在游戏后面，看起来就是「程序点了没反应」。
        fit_client_size(hwnd, client_w, client_h);
        center_on_work_area(hwnd);
        let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
        let _ = UpdateWindow(hwnd);
        // 界面开着期间保持置顶：目标窗口常是「无边框全屏」的游戏，
        // 不置顶的话界面会被压在游戏后面，鼠标点不到（看起来像程序卡死）。
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
        );
        let _ = SetForegroundWindow(hwnd);
        let _ = BringWindowToTop(hwnd);
        if let Ok(list) = GetDlgItem(Some(hwnd), ID_LIST) {
            let _ = SetFocus(Some(list));
        }
        {
            let (mut client, mut outer) = (RECT::default(), RECT::default());
            let _ = GetClientRect(hwnd, &mut client);
            let _ = GetWindowRect(hwnd, &mut outer);
            log::info!(
                "选择界面：dpi={dpi} 客户区 {}x{}，窗口 {}x{} @ ({},{})，前台窗口 {:?}，本窗口 {:?}",
                client.right - client.left,
                client.bottom - client.top,
                outer.right - outer.left,
                outer.bottom - outer.top,
                outer.left,
                outer.top,
                GetForegroundWindow(),
                hwnd
            );
        }
        // 退出只看 CLOSED（WM_DESTROY）。这里不用 GetMessageW + WM_QUIT：
        // 别的窗口留下的 WM_QUIT 会让界面刚打开就自己关掉。
        let mut msg = MSG::default();
        loop {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                // 残留的 WM_QUIT 直接丢掉
                if msg.message == WM_QUIT {
                    continue;
                }
                // 让 Tab 切换焦点、回车触发「开始渲染」——普通窗口不加这一步只能靠鼠标点
                if IsDialogMessageW(hwnd, &mut msg).as_bool() {
                    continue;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            if CLOSED.load(Ordering::SeqCst) {
                break;
            }
            // 队列空了就阻塞等新消息，别空转
            if WaitMessage().is_err() {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }

        log::info!("选择界面：消息循环结束（已确认 = {}）", confirmed_slot().is_some());
        // 关掉界面（没点「开始渲染」）时这里就是 None
        let Some(c) = confirmed_slot().take() else {
            return Ok(None);
        };
        Ok(Some(Picked {
            hwnd: HWND(c.hwnd as *mut core::ffi::c_void),
            title: c.title,
            model_path: c.model_path,
            model_input: c.model_input,
            output_size: c.output_size,
        }))
    }
}
