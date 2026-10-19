//! 采集 + 呈现层：按窗口采集（Windows Graphics Capture）→ 直通推理 → swapchain 垂直同步呈现
//!
//! 采集优先走 WGC：按 HWND 抓这个窗口自己的画面，窗口挪动/改尺寸自动跟随、
//! 被别的窗口盖住也照样能采；失败时退回桌面复制（DXGI Desktop Duplication）。
//!
//! 整条链路里 CPU 只做三件事：提交 CS、等待围栏、Present(1)。
//! 没有 `Map`、没有 `memcpy`、没有 OpenCV 窗口重绘，因此不会出现 CPU 版那种「一顿一顿」的观感。

use crate::gpu_style::GpuStylePipeline;
use crate::window_picker::Picked;
use std::sync::atomic::{AtomicBool, Ordering};
use windows::core::{Interface, PCWSTR};
use windows::Win32::Foundation::{
    ERROR_CLASS_ALREADY_EXISTS, GetLastError, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::Direct3D11::{ID3D11RenderTargetView, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Gdi::{ClientToScreen, UpdateWindow};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_ESCAPE};
use windows::Win32::UI::WindowsAndMessaging::*;

/// 上屏窗口标题
const DISPLAY_TITLE: &str = "AnimeGAN GPU Passthrough";

/// 上屏窗口被关掉（点 X）时置位。
///
/// 这里**不能**用 `PostQuitMessage`：那条 WM_QUIT 会留在本线程的消息队列里，
/// 等下一轮启动界面/上屏循环去取，结果就是「刚打开就被顶掉」或者整程序退出。
static CLOSED: AtomicBool = AtomicBool::new(false);

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                CLOSED.store(true, Ordering::SeqCst);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

/// 退出时（Esc / 点 X / 中途出错）一定把上屏窗口关掉，否则会留下一个没人管的窗口
struct DisplayWindow(HWND);

impl Drop for DisplayWindow {
    fn drop(&mut self) {
        unsafe {
            if IsWindow(Some(self.0)).as_bool() {
                let _ = DestroyWindow(self.0);
            }
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 创建一个用于显示风格化结果的普通窗口；`client_w/client_h` 是**客户区**尺寸。
/// 位置交给系统默认（CW_USEDEFAULT），不再按显示器挑位置。
fn create_window(title: &str, client_w: i32, client_h: i32) -> Result<HWND, String> {
    unsafe {
        let instance = GetModuleHandleW(None).map_err(|e| format!("GetModuleHandleW: {e}"))?;
        let class_name = wide("AnimationImageeGpuWindow");
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: windows::Win32::Foundation::HINSTANCE(instance.0),
            lpszClassName: PCWSTR::from_raw(class_name.as_ptr()),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        // 第二次运行（回到选择界面再进来）时这个类已经注册过，不算错误
        if RegisterClassW(&wc) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
            return Err("RegisterClassW 失败".into());
        }
        // CreateWindowExW 的宽高是「整个窗口」，不减掉边框会让客户区比目标小一圈，
        // 上屏时又多一次缩放（画面会轻微变糊）。
        let style = WS_OVERLAPPEDWINDOW;
        let mut outer = RECT {
            left: 0,
            top: 0,
            right: client_w,
            bottom: client_h,
        };
        let _ = AdjustWindowRectEx(&mut outer, style, false, WINDOW_EX_STYLE(0));
        let (outer_w, outer_h) = (outer.right - outer.left, outer.bottom - outer.top);
        let title = wide(title);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR::from_raw(class_name.as_ptr()),
            PCWSTR::from_raw(title.as_ptr()),
            style | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            outer_w,
            outer_h,
            None,
            None,
            Some(windows::Win32::Foundation::HINSTANCE(instance.0)),
            None,
        )
        .map_err(|e| format!("CreateWindowExW: {e}"))?;
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = UpdateWindow(hwnd);
        // 位置交给系统后可能正好开在别的窗口后面，抬到前面来一次
        let _ = SetForegroundWindow(hwnd);
        let _ = BringWindowToTop(hwnd);
        // 跨 DPI 时上面按系统 DPI 估的边框不准，量一次实际值把客户区补正
        resize_window_client(hwnd, client_w, client_h);
        Ok(hwnd)
    }
}

/// 窗口标题（日志用）
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

/// 模型输入留空时按窗口客户区尺寸推理；取 8 的倍数（模型的上下采样是 2/4 的倍数，
/// 奇数尺寸容易在边界出接缝），最小 64
fn align8(v: u32) -> u32 {
    (v.max(64) / 8 * 8).max(64)
}

/// 当前窗口客户区尺寸
fn window_client_size(hwnd: HWND) -> (u32, u32) {
    unsafe {
        let mut rect = RECT::default();
        if GetClientRect(hwnd, &mut rect).is_err() {
            return (0, 0);
        }
        (
            (rect.right - rect.left).max(0) as u32,
            (rect.bottom - rect.top).max(0) as u32,
        )
    }
}

/// 窗口的「可见」矩形（DWM 扩展边框，不含隐藏的拖拽边框）
fn visible_frame_rect(hwnd: HWND) -> Option<RECT> {
    use windows::Win32::Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
    unsafe {
        let mut rect = RECT::default();
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut rect as *mut RECT as *mut _,
            std::mem::size_of::<RECT>() as u32,
        )
        .ok()?;
        Some(rect)
    }
}

/// 把窗口**客户区**调整成指定尺寸（窗口中心保持不动）。
///
/// 不用 `AdjustWindowRectEx` 估算：它按系统 DPI 算边框，窗口落在另一块不同缩放的
/// 显示器上时客户区会差出几十像素（画面于是被拉变形），这里直接量实际边框厚度再补。
fn resize_window_client(hwnd: HWND, client_w: i32, client_h: i32) {
    unsafe {
        for _ in 0..3 {
            let mut outer = RECT::default();
            let mut client = RECT::default();
            if GetWindowRect(hwnd, &mut outer).is_err() || GetClientRect(hwnd, &mut client).is_err() {
                return;
            }
            let cur_w = client.right - client.left;
            let cur_h = client.bottom - client.top;
            if cur_w == client_w && cur_h == client_h {
                return;
            }
            let dx = (outer.right - outer.left) - cur_w;
            let dy = (outer.bottom - outer.top) - cur_h;
            let cx = (outer.left + outer.right) / 2;
            let cy = (outer.top + outer.bottom) / 2;
            let w = (client_w + dx).max(2);
            let h = (client_h + dy).max(2);
            if SetWindowPos(
                hwnd,
                None,
                cx - w / 2,
                cy - h / 2,
                w,
                h,
                SWP_NOZORDER | SWP_NOACTIVATE,
            )
            .is_err()
            {
                return;
            }
        }
    }
}

/// 窗口客户区在桌面坐标系下的矩形
fn client_rect_on_desktop(hwnd: HWND) -> Result<RECT, String> {
    unsafe {
        let mut rect = RECT::default();
        GetClientRect(hwnd, &mut rect).map_err(|e| format!("GetClientRect: {e}"))?;
        let mut origin = POINT { x: 0, y: 0 };
        if !ClientToScreen(hwnd, &mut origin).as_bool() {
            return Err("ClientToScreen 失败".into());
        }
        Ok(RECT {
            left: origin.x,
            top: origin.y,
            right: origin.x + (rect.right - rect.left),
            bottom: origin.y + (rect.bottom - rect.top),
        })
    }
}

// 下面这套「桌面复制 + HDR 白点」只给 cargo test 里的手动诊断用；
// 程序本身只做按窗口直通（WGC），不支持就直接提示退出，不再退回桌面复制。
#[cfg(test)]
fn contains(rect: &RECT, p: POINT) -> bool {
    p.x >= rect.left && p.x < rect.right && p.y >= rect.top && p.y < rect.bottom
}

/// 找到包含指定窗口的显示器输出，并在其上开启桌面复制（仅测试用）。
/// 第三个返回值是该显示器的 GDI 名（如 `\\.\DISPLAY1`），用于查询 HDR SDR 白点。
#[cfg(test)]
pub(crate) fn create_duplication(
    device11: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
    center: POINT,
) -> Result<(IDXGIOutputDuplication, RECT, String), String> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1::<IDXGIFactory1>()
            .map_err(|e| format!("CreateDXGIFactory1: {e}"))?;
        let mut index = 0u32;
        loop {
            let adapter = match factory.EnumAdapters1(index) {
                Ok(a) => a,
                Err(_) => break,
            };
            let mut output_index = 0u32;
            loop {
                let output = match adapter.EnumOutputs(output_index) {
                    Ok(o) => o,
                    Err(_) => break,
                };
                let desc = output.GetDesc().map_err(|e| format!("Output::GetDesc: {e}"))?;
                if !contains(&desc.DesktopCoordinates, center) {
                    output_index += 1;
                    continue;
                }
                let output1: IDXGIOutput1 = output
                    .cast()
                    .map_err(|e| format!("IDXGIOutput1: {e}"))?;
                let dup = output1
                    .DuplicateOutput(device11)
                    .map_err(|e| format!("DuplicateOutput: {e}"))?;
                let name = String::from_utf16_lossy(&desc.DeviceName)
                    .trim_end_matches('\0')
                    .to_string();
                return Ok((dup, desc.DesktopCoordinates, name));
            }
            index += 1;
        }
    }
    Err("没有找到包含游戏窗口的显示器（多显卡/远程桌面下可能不支持）".into())
}

/// 每帧把「采集纹理 + 裁剪区域」交给回调；返回本帧是否有新内容
type FrameFn<'a> = dyn FnMut(&ID3D11Texture2D, u32, u32, u32, u32) -> Result<(), String> + 'a;

/// 按窗口采集：Windows Graphics Capture（Win10 1809+）。
///
/// 和桌面复制的本质区别：WGC 抓的是「这个窗口自己的画面」，所以
/// - 窗口改大小/挪位置自动跟随（`ContentSize` 每帧都可能变）；
/// - 被别的窗口盖住、只露出一角、甚至切到后台都不会丢画面；
/// - 不会有「桌面合成里混进我们自己的窗口」的套娃问题。
struct WindowCapture {
    pool: windows::Graphics::Capture::Direct3D11CaptureFramePool,
    // 持有 session / item 才会持续推帧，别被 drop
    _session: windows::Graphics::Capture::GraphicsCaptureSession,
    _item: windows::Graphics::Capture::GraphicsCaptureItem,
    game: HWND,
}

impl WindowCapture {
    fn new(
        device11: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
        hwnd: HWND,
    ) -> Result<Self, String> {
        use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem};
        use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
        use windows::Graphics::DirectX::DirectXPixelFormat;
        use windows::Win32::System::WinRT::Direct3D11::CreateDirect3D11DeviceFromDXGIDevice;
        use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
        use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};

        unsafe {
            // WinRT：重复初始化返回 RPC_E_CHANGED_MODE 也无所谓，只要已初始化就能用
            let _ = RoInitialize(RO_INIT_MULTITHREADED);
            let interop: IGraphicsCaptureItemInterop =
                windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
                    .map_err(|e| format!("IGraphicsCaptureItemInterop: {e}"))?;
            let item: GraphicsCaptureItem = interop
                .CreateForWindow(hwnd)
                .map_err(|e| format!("CreateForWindow: {e}"))?;
            let size = item.Size().map_err(|e| format!("GraphicsCaptureItem::Size: {e}"))?;
            // 用管线自己的 D3D11 设备包装成 WinRT 设备：帧纹理直接落在同一张卡上，零拷贝
            let dxgi_device: IDXGIDevice = device11.cast().map_err(|e| format!("IDXGIDevice: {e}"))?;
            let inspectable = CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device)
                .map_err(|e| format!("CreateDirect3D11DeviceFromDXGIDevice: {e}"))?;
            let rt_device: IDirect3DDevice = inspectable
                .cast()
                .map_err(|e| format!("IDirect3DDevice: {e}"))?;
            let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
                &rt_device,
                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                2,
                size,
            )
            .map_err(|e| format!("CreateFreeThreaded: {e}"))?;
            let session = pool
                .CreateCaptureSession(&item)
                .map_err(|e| format!("CreateCaptureSession: {e}"))?;
            // 不抓系统光标：模型输入应当是游戏自己的原始输出，混进桌面光标就变了
            let _ = session.SetIsCursorCaptureEnabled(false);
            // Win11 的黄色采集边框，能关就关（老系统上这个调用会失败，忽略）
            let _ = session.SetIsBorderRequired(false);
            session.StartCapture().map_err(|e| format!("StartCapture: {e}"))?;
            Ok(Self {
                pool,
                _session: session,
                _item: item,
                game: hwnd,
            })
        }
    }

    fn with_frame(&mut self, f: &mut FrameFn<'_>) -> Result<bool, String> {
        use windows::Win32::System::WinRT::Direct3D11::IDirect3DDxgiInterfaceAccess;
        unsafe {
            // 没有新帧时这里会返回错误/空指针，正常情况，不算失败
            let frame = match self.pool.TryGetNextFrame() {
                Ok(frame) => frame,
                Err(_) => return Ok(false),
            };
            let access: IDirect3DDxgiInterfaceAccess = frame
                .Surface()
                .map_err(|e| format!("Direct3D11CaptureFrame::Surface: {e}"))?
                .cast()
                .map_err(|e| format!("IDirect3DDxgiInterfaceAccess: {e}"))?;
            let tex: ID3D11Texture2D = access
                .GetInterface()
                .map_err(|e| format!("IDirect3DDxgiInterfaceAccess::GetInterface: {e}"))?;
            let size = frame
                .ContentSize()
                .map_err(|e| format!("ContentSize: {e}"))?;
            let (w, h) = (size.Width.max(1) as u32, size.Height.max(1) as u32);
            // WGC 抓的是「整个窗口」的画面（含标题栏和边框），这里按客户区在窗口里的
            // 偏移裁出客户区，才能和结果窗口 1:1 对上（否则顶上多一条标题栏、底下少一截）。
            let (left, top, mut cw, mut ch) =
                match (client_rect_on_desktop(self.game), visible_frame_rect(self.game)) {
                    (Ok(c), Some(f)) => {
                        let cw = (c.right - c.left).max(1) as u32;
                        let ch = (c.bottom - c.top).max(1) as u32;
                        if cw == w && ch == h {
                            (0, 0, cw, ch)
                        } else {
                            let ox = (c.left - f.left).max(0) as u32;
                            let oy = (c.top - f.top).max(0) as u32;
                            (ox.min(w - 1), oy.min(h - 1), cw, ch)
                        }
                    }
                    _ => (0, 0, w, h),
                };
            // 尺寸在极端情况下（窗口刚改尺寸、帧还是旧的）可能比纹理大，兜到纹理范围内，
            // 否则 upload_texture 会每帧刷一条「裁剪区域被裁到…」
            cw = cw.clamp(1, w - left);
            ch = ch.clamp(1, h - top);
            f(&tex, left, top, cw, ch)?;
            Ok(true)
        }
    }
}

/// 显示器的 HDR 信息（仅测试用）
#[cfg(test)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct HdrInfo {
    /// 该显示器当前是否处于 HDR 模式（此时采集到的是 scRGB 线性值）
    pub is_hdr: bool,
    /// scRGB 线性值 → 「SDR 白 = 1.0」的归一化乘数（80 / SDR 白点亮度）
    pub white_scale: f32,
}

#[cfg(test)]
impl Default for HdrInfo {
    fn default() -> Self {
        Self {
            is_hdr: false,
            white_scale: 1.0,
        }
    }
}

/// 查询某个显示器（GDI 名，如 `\\.\DISPLAY1`）是否在 HDR 模式，以及 scRGB 的归一化乘数。
///
/// HDR 下采集到的是 scRGB 线性值：1.0 = 80nits，而 SDR 内容的白点由系统的
/// 「SDR 内容亮度」决定（常见 200nits ≈ 2.5），不除这个系数就会整体抬亮、
/// 高光切白，画面（以及喂给模型的输入）都会泛白发闷。
#[cfg(test)]
pub(crate) fn hdr_info(gdi_name: &str) -> HdrInfo {
    if !output_is_hdr(gdi_name) {
        return HdrInfo::default();
    }
    HdrInfo {
        is_hdr: true,
        // 查不到白点时退回 1.0（画面偏亮，但不会更糟）
        white_scale: query_sdr_white_scale(gdi_name).unwrap_or(1.0),
    }
}

/// 输出的色彩描述（仅测试用：判断 HDR / 打印诊断信息）
#[cfg(test)]
#[allow(dead_code)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct OutputColorInfo {
    /// DXGI_COLOR_SPACE_TYPE 的原始数值：0 = G22_P709（SDR），12 = G10_P709（scRGB 线性），
    /// 24 = G2084_P2020（HDR10）
    pub color_space: i32,
    pub max_luminance: f32,
    pub max_full_frame_luminance: f32,
}

#[cfg(test)]
pub(crate) fn output_color_info(gdi_name: &str) -> Option<OutputColorInfo> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1::<IDXGIFactory1>().ok()?;
        let mut index = 0u32;
        while let Ok(adapter) = factory.EnumAdapters1(index) {
            let mut output_index = 0u32;
            while let Ok(output) = adapter.EnumOutputs(output_index) {
                if let Ok(desc) = output.GetDesc() {
                    let name = String::from_utf16_lossy(&desc.DeviceName);
                    if name.trim_end_matches('\0').eq_ignore_ascii_case(gdi_name) {
                        let d1 = output.cast::<IDXGIOutput6>().ok()?.GetDesc1().ok()?;
                        return Some(OutputColorInfo {
                            color_space: d1.ColorSpace.0,
                            max_luminance: d1.MaxLuminance,
                            max_full_frame_luminance: d1.MaxFullFrameLuminance,
                        });
                    }
                }
                output_index += 1;
            }
            index += 1;
        }
    }
    None
}

/// 用 DXGI_OUTPUT_DESC1.ColorSpace 判断该显示器是否 HDR
#[cfg(test)]
fn output_is_hdr(gdi_name: &str) -> bool {
    use windows::Win32::Graphics::Dxgi::Common::{
        DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709, DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020,
    };
    match output_color_info(gdi_name) {
        Some(info) => {
            info.color_space == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020.0
                || info.color_space == DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709.0
        }
        None => false,
    }
}

/// 查询显示器当前的 SDR 白点，返回 scRGB 归一化乘数（80 / 白点亮度）
#[cfg(test)]
fn query_sdr_white_scale(gdi_name: &str) -> Option<f32> {
    use windows::Win32::Devices::Display::*;
    unsafe {
        let mut num_paths = 0u32;
        let mut num_modes = 0u32;
        if GetDisplayConfigBufferSizes(
            QDC_ONLY_ACTIVE_PATHS,
            &mut num_paths,
            &mut num_modes,
        ) != windows::Win32::Foundation::WIN32_ERROR(0)
        {
            return None;
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); num_paths as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); num_modes as usize];
        if QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut num_paths,
            paths.as_mut_ptr(),
            &mut num_modes,
            modes.as_mut_ptr(),
            None,
        ) != windows::Win32::Foundation::WIN32_ERROR(0)
        {
            return None;
        }
        for path in paths.iter().take(num_paths as usize) {
            let mut src = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
            src.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
            src.header.size = size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
            src.header.adapterId = path.sourceInfo.adapterId;
            src.header.id = path.sourceInfo.id;
            if DisplayConfigGetDeviceInfo(&mut src.header) != 0 {
                continue;
            }
            let name = String::from_utf16_lossy(&src.viewGdiDeviceName);
            if !name.trim_end_matches('\0').eq_ignore_ascii_case(gdi_name) {
                continue;
            }
            let mut wl = DISPLAYCONFIG_SDR_WHITE_LEVEL::default();
            wl.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL;
            wl.header.size = size_of::<DISPLAYCONFIG_SDR_WHITE_LEVEL>() as u32;
            wl.header.adapterId = path.targetInfo.adapterId;
            wl.header.id = path.targetInfo.id;
            if DisplayConfigGetDeviceInfo(&mut wl.header) != 0 {
                return None;
            }
            // SDRWhiteLevel 的单位是 80nits 的千分之一，所以 80/nits = 1000/SDRWhiteLevel
            let scale = 1000.0 / wl.SDRWhiteLevel as f32;
            return if scale.is_finite() && scale > 0.0 {
                Some(scale)
            } else {
                None
            };
        }
    }
    None
}

struct Swapchain {
    swap: IDXGISwapChain1,
    rtv: Option<ID3D11RenderTargetView>,
    width: u32,
    height: u32,
}

impl Swapchain {
    fn new(
        device11: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
        hwnd: HWND,
        width: u32,
        height: u32,
    ) -> Result<Self, String> {
        unsafe {
            let device: windows::Win32::Graphics::Dxgi::IDXGIDevice = device11
                .cast()
                .map_err(|e| format!("cast IDXGIDevice: {e}"))?;
            let adapter = device.GetAdapter().map_err(|e| format!("GetAdapter: {e}"))?;
            let factory: IDXGIFactory2 = adapter
                .GetParent()
                .map_err(|e| format!("GetParent(IDXGIFactory2): {e}"))?;
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: width,
                Height: height,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                Stereo: false.into(),
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                AlphaMode: DXGI_ALPHA_MODE_IGNORE,
                Flags: 0,
            };
            // CreateSwapChainForHwnd 传 D3D11 设备（内部会取立即上下文）
            let swap = factory
                .CreateSwapChainForHwnd(device11, hwnd, &desc, None, None)
                .map_err(|e| format!("CreateSwapChainForHwnd: {e}"))?;
            let rtv = create_rtv(device11, &swap)?;
            Ok(Self {
                swap,
                rtv: Some(rtv),
                width,
                height,
            })
        }
    }

    fn resize(&mut self, device11: &windows::Win32::Graphics::Direct3D11::ID3D11Device, w: u32, h: u32) -> Result<(), String> {
        if w == 0 || h == 0 || (w == self.width && h == self.height) {
            return Ok(());
        }
        unsafe {
            self.rtv = None;
            self.swap
                .ResizeBuffers(2, w, h, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SWAP_CHAIN_FLAG(0))
                .map_err(|e| format!("ResizeBuffers: {e}"))?;
            self.rtv = Some(create_rtv(device11, &self.swap)?);
        }
        self.width = w;
        self.height = h;
        Ok(())
    }
}

unsafe fn create_rtv(
    device11: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
    swap: &IDXGISwapChain1,
) -> Result<ID3D11RenderTargetView, String> {
    unsafe {
        let backbuffer: ID3D11Texture2D = swap
            .GetBuffer(0)
            .map_err(|e| format!("SwapChain::GetBuffer: {e}"))?;
        let mut rtv: Option<ID3D11RenderTargetView> = None;
        device11
            .CreateRenderTargetView(&backbuffer, None, Some(&mut rtv))
            .map_err(|e| format!("CreateRenderTargetView: {e}"))?;
        rtv.ok_or_else(|| "RTV 为空".to_string())
    }
}

pub fn run(picked: &Picked) -> Result<(), String> {
    // 1) 目标窗口由启动界面选定，这里只做一次有效性检查
    let game = picked.hwnd;
    if game.0.is_null() || !unsafe { IsWindow(Some(game)) }.as_bool() {
        return Err("所选窗口已经关闭，请重新选择".into());
    }

    let client = client_rect_on_desktop(game)?;
    let (client_w, client_h) = (
        (client.right - client.left).max(0) as u32,
        (client.bottom - client.top).max(0) as u32,
    );
    let game_title = if picked.title.is_empty() {
        window_title_of(game)
    } else {
        picked.title.clone()
    };

    // 2) 模型输入：界面里填了就优先用用户填的（固定输入尺寸的模型必须原样）；
    //    没填就直接用目标窗口客户区的宽高
    let (infer_w, infer_h) = match picked.model_input {
        Some(size) => size,
        None => (align8(client_w), align8(client_h)),
    };
    log::info!(
        "上屏开始：目标窗口「{game_title}」客户区 {client_w}x{client_h}，模型 {}，请求输入 {infer_w}x{infer_h}（{}）",
        picked.model_path,
        if picked.model_input.is_some() {
            "界面填写"
        } else {
            "留空，取目标窗口客户区尺寸"
        }
    );

    // 3) 先建结果窗口：点「开始渲染」后马上有反馈，加载模型/预热期间不会一片空白
    let follow_game = picked.output_size.is_none();
    let (out_w, out_h) = picked.output_size.unwrap_or((client_w, client_h));
    CLOSED.store(false, Ordering::SeqCst);
    let hwnd = create_window(
        "AnimeGAN GPU Passthrough（正在初始化…）",
        out_w.max(2) as i32,
        out_h.max(2) as i32,
    )?;
    // 之后任何 return（含 ? 早期出错）都会把窗口关掉，不留幽灵窗口
    let _display = DisplayWindow(hwnd);
    let (disp_w, disp_h) = {
        let (w, h) = window_client_size(hwnd);
        (w.max(2), h.max(2))
    };
    log::info!("上屏：显示窗口 {disp_w}x{disp_h} 已创建，开始加载模型");

    let mut pipeline = GpuStylePipeline::new(&picked.model_path, infer_w, infer_h)?;
    log::info!(
        "上屏：模型管线就绪（实际输入 {}x{}，模型固定尺寸时以模型为准），创建交换链",
        pipeline.width,
        pipeline.height
    );
    let mut swap = Swapchain::new(pipeline.device11(), hwnd, disp_w, disp_h)?;
    unsafe {
        let title = wide(DISPLAY_TITLE);
        let _ = SetWindowTextW(hwnd, PCWSTR::from_raw(title.as_ptr()));
    }

    // 只做按窗口采集（WGC）。拿不到就直接失败退出，不去做桌面复制兜底
    log::info!("上屏：初始化采集（WGC）");
    let mut capture = WindowCapture::new(pipeline.device11(), game).map_err(|e| {
        format!(
            "按窗口采集（WGC）不可用：{e}\n\n这个程序只做直通上屏，不支持时不会退回桌面复制。\n\
             需要 Windows 10 1809 及以上，并且目标窗口不能有跨进程的内容保护。"
        )
    })?;
    // WGC 给的是窗口自己的 SDR 画面（和游戏原始输出一致），不做任何色彩转换
    pipeline.set_sdr_white_scale(1.0);
    log::info!("上屏：采集方式 = 按窗口（WGC），跟随窗口尺寸/位置变化，被遮挡也能采");

    // 跟随游戏窗口尺寸：客户区一变就把结果窗口改成同样大小
    let mut game_client = (
        (client.right - client.left).max(0),
        (client.bottom - client.top).max(0),
    );

    // 预热：DirectML 首次运行会编译算子（~1s），先跑一帧把编译成本移出主循环
    log::info!("上屏：预热 DirectML（首次编译算子，可能几秒）");
    let dummy = DummyTexture::new(pipeline.device11())?;
    for _ in 0..2 {
        // 用 dummy 自己的尺寸，避免触发「裁剪区域越界」的提示
        pipeline.upload_texture(dummy.as_texture(), 0, 0, 16, 16)?;
        pipeline.infer()?;
    }
    log::info!("上屏：预热完成，进入主循环（按 Esc 或关闭窗口退出）");

    let mut frames = 0u64;
    let mut last_report = std::time::Instant::now();
    let mut infer_ms_sum = 0.0f32;

    'main: loop {
        // 1) 消息泵：退出只认「上屏窗口被关」或 Esc，残留的 WM_QUIT 一律丢掉
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    continue;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            if CLOSED.load(Ordering::SeqCst) {
                break 'main;
            }
            if GetAsyncKeyState(VK_ESCAPE.0 as i32) < 0 {
                break 'main;
            }
        }

        // 2) 游戏窗口尺寸变了 → 结果窗口跟着变（游戏全屏/窗口化/拖边都算）
        //    用户在界面里指定了上屏尺寸时不跟随，保持固定大小
        if follow_game {
            if let Ok(c) = client_rect_on_desktop(game) {
                let size = ((c.right - c.left).max(0), (c.bottom - c.top).max(0));
                if size.0 >= 32 && size.1 >= 32 && size != game_client {
                    game_client = size;
                    resize_window_client(hwnd, size.0 as i32, size.1 as i32);
                    eprintln!("目标窗口变为 {}x{}，结果窗口跟随", size.0, size.1);
                }
            }
        }

        // 3) 取最新一帧（没有新帧就复用上一帧结果，保证刷新节奏稳定）
        let got_frame = {
            let mut upload = |tex: &ID3D11Texture2D, l: u32, t: u32, w: u32, h: u32| {
                pipeline.upload_texture(tex, l, t, w, h)?;
                // 必须在帧对象释放之前确保 CS 已读完这张纹理
                pipeline.sync()
            };
            capture.with_frame(&mut upload)?
        };

        // 4) 有新帧才推理（推理内部已包含显存同步），否则直接复用上一帧结果
        if got_frame {
            let started = std::time::Instant::now();
            pipeline.infer()?;
            infer_ms_sum += started.elapsed().as_secs_f32() * 1000.0;
            frames += 1;
        }

        // 5) 窗口被拖动缩放（或刚跟随游戏窗口改过尺寸）时重建后缓冲
        {
            let (w, h) = window_client_size(hwnd);
            if w > 0 && h > 0 {
                if let Err(e) = swap.resize(pipeline.device11(), w, h) {
                    eprintln!("重建后缓冲失败: {e}");
                }
            }
        }

        // 6) 每帧都上屏 + 垂直同步，避免画面跳变
        let rtv = swap.rtv.as_ref().ok_or("渲染目标未就绪")?;
        pipeline.present(rtv, swap.width, swap.height)?;
        unsafe {
            let hr = swap.swap.Present(1, DXGI_PRESENT(0));
            if hr.is_err() {
                return Err(format!("Present 失败: {hr:?}"));
            }
        }

        if last_report.elapsed().as_secs_f32() >= 1.0 {
            let fps = frames as f32 / last_report.elapsed().as_secs_f32();
            let avg = if frames > 0 { infer_ms_sum / frames as f32 } else { 0.0 };
            eprintln!("fps≈{fps:.1} | 单帧推理+搬运 {avg:.1}ms");
            frames = 0;
            infer_ms_sum = 0.0;
            last_report = std::time::Instant::now();
        }
    }

    log::info!("上屏：主循环退出，销毁显示窗口");
    Ok(())
}

/// 预热用的纯色纹理（避免依赖游戏窗口）
struct DummyTexture {
    tex: ID3D11Texture2D,
}

impl DummyTexture {
    fn new(device11: &windows::Win32::Graphics::Direct3D11::ID3D11Device) -> Result<Self, String> {
        unsafe {
            let mut tex: Option<ID3D11Texture2D> = None;
            device11
                .CreateTexture2D(
                    &windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE2D_DESC {
                        Width: 16,
                        Height: 16,
                        MipLevels: 1,
                        ArraySize: 1,
                        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                        SampleDesc: DXGI_SAMPLE_DESC {
                            Count: 1,
                            Quality: 0,
                        },
                        Usage: windows::Win32::Graphics::Direct3D11::D3D11_USAGE_DEFAULT,
                        BindFlags: windows::Win32::Graphics::Direct3D11::D3D11_BIND_SHADER_RESOURCE
                            .0 as u32,
                        CPUAccessFlags: 0,
                        MiscFlags: 0,
                    },
                    None,
                    Some(&mut tex),
                )
                .map_err(|e| format!("预热纹理创建失败: {e}"))?;
            Ok(Self {
                tex: tex.ok_or("预热纹理为空")?,
            })
        }
    }

    fn as_texture(&self) -> &ID3D11Texture2D {
        &self.tex
    }
}

/// 自检：开窗口 + 交换链，用桌面采集的实时画面跑固定帧数后退出（验证上屏链路）
#[cfg(test)]
pub(crate) fn selftest_frames(frames: u32) -> Result<f32, String> {
    let mut pipeline = GpuStylePipeline::new("AnimeGANv3_Hayao_36.onnx", 512, 288)?;
    let hwnd = create_window("AnimeGAN GPU selftest", 1024, 576)?;
    let swap = Swapchain::new(pipeline.device11(), hwnd, 1024, 576)?;
    let (dup, monitor, _) = create_duplication(pipeline.device11(), POINT { x: 400, y: 300 })?;
    let w = (monitor.right - monitor.left) as u32;
    let h = (monitor.bottom - monitor.top) as u32;
    let mut total = 0.0f32;
    for i in 0..frames {
        unsafe {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            if dup.AcquireNextFrame(16, &mut info, &mut resource).is_ok() {
                if let Some(res) = resource {
                    if let Ok(tex) = res.cast::<ID3D11Texture2D>() {
                        pipeline.upload_texture(&tex, 0, 0, w, h)?;
                        pipeline.sync()?;
                    }
                }
                let _ = dup.ReleaseFrame();
            }
        }
        let started = std::time::Instant::now();
        pipeline.infer()?;
        if i > 1 {
            total += started.elapsed().as_secs_f32() * 1000.0;
        }
        let rtv = swap.rtv.as_ref().ok_or("渲染目标未就绪")?;
        pipeline.present(rtv, swap.width, swap.height)?;
        // 定位「画面偏暗」这类颜色问题时用：同一帧分别导出模型输出和即将上屏的像素
        if i == 20 && std::env::var_os("ANIME_DUMP").is_some() {
            pipeline.dump_output_png("target/check_selftest_model.png")?;
            let backbuffer: ID3D11Texture2D = unsafe { swap.swap.GetBuffer(0) }
                .map_err(|e| format!("GetBuffer: {e}"))?;
            pipeline.dump_bgra_texture(&backbuffer, "target/check_selftest_present.png")?;
        }
        unsafe {
            let hr = swap.swap.Present(1, DXGI_PRESENT(0));
            if hr.is_err() {
                return Err(format!("Present 失败: {hr:?}"));
            }
        }
    }
    let _ = unsafe { DestroyWindow(hwnd) };
    Ok(if frames > 2 { total / (frames - 2) as f32 } else { total })
}
