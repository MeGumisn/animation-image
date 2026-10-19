// 没有黑控制台：日志统一写 logs/animegan_rCURRENT.log（见 app_log）
#![windows_subsystem = "windows"]

mod app_log;
mod gpu_style;
mod gpu_window;
mod window_picker;

/// 没有控制台，出错时弹个对话框：只写日志的话用户看到的就是「点了一下没反应」
fn show_error(text: &str) {
    use windows::core::PCWSTR;
    use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    let title: Vec<u16> = "AnimeGAN - 出错了\0".encode_utf16().collect();
    let body: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        MessageBoxW(
            None,
            PCWSTR::from_raw(body.as_ptr()),
            PCWSTR::from_raw(title.as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}

fn main() {
    app_log::init();

    // 正常退出时清掉「运行中」标记；panic/崩溃/被强杀时清不掉，下次启动会报异常退出
    struct CleanExit;
    impl Drop for CleanExit {
        fn drop(&mut self) {
            app_log::mark_clean_exit();
        }
    }
    let _clean_exit = CleanExit;

    // 必须在任何窗口/坐标计算之前声明 DPI 感知：
    // 否则 GetClientRect/ClientToScreen 返回的是被系统缩放的「虚拟像素」，
    // 而采集拿到的是物理像素，两者不一致会导致只采到一部分并且发糊。
    unsafe {
        use windows::Win32::UI::HiDpi::{
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetAwarenessFromDpiAwarenessContext,
            GetThreadDpiAwarenessContext, SetProcessDpiAwarenessContext,
        };
        // 进程在加载 DLL（opencv 等）时可能已经把 DPI 感知设好了，这一步返回 ACCESS_DENIED 是正常的；
        // 只要最终是「感知」状态（≥1），GetClientRect/ClientToScreen 返回的就是物理像素。
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let awareness = GetAwarenessFromDpiAwarenessContext(GetThreadDpiAwarenessContext()).0;
        if awareness < 1 {
            log::warn!("进程不是 DPI 感知的（awareness={awareness}），采集坐标可能偏移");
        }
    }

    let mut defaults = window_picker::Defaults::default();

    // 关掉上屏窗口就回到选择界面，可以再挑别的窗口；
    // 只有「取消选择」才正常退出，直通失败则提示后退出（不做任何兜底）
    loop {
        let picked = match window_picker::pick(&defaults) {
            Ok(Some(p)) => p,
            Ok(None) => {
                log::info!("取消选择，程序退出");
                break;
            }
            Err(e) => {
                log::error!("打开选择界面失败: {e}");
                show_error(&format!("无法打开选择界面：\n{e}"));
                break;
            }
        };

        log::info!(
            "开始渲染：hwnd={:?}「{}」模型={} 输入={:?} 上屏={:?}",
            picked.hwnd,
            picked.title,
            picked.model_path,
            picked.model_input,
            picked.output_size
        );

        // 下一轮用这次的配置作为默认值，省得每次重新填
        defaults.model_path = picked.model_path.clone();
        defaults.model_input = picked.model_input;
        defaults.output_size = picked.output_size;

        match gpu_window::run(&picked) {
            Ok(()) => log::info!("上屏窗口已关闭，回到选择界面"),
            Err(e) => {
                // 直通不支持就没有别的路可走：提示原因然后退出
                log::error!("直通失败: {e}");
                show_error(&format!("无法直通上屏：\n\n{e}"));
                break;
            }
        }
    }

    log::info!("程序退出（正常）");
}
