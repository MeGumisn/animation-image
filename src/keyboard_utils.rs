use rand::random_range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::sleep;
use std::time::Duration;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::VK_4;
use windows::Win32::UI::WindowsAndMessaging::{SendMessageW, WM_KEYDOWN};

pub struct WindowsKeyboard {
    hwnd: HWND,
}

static IS_RUNNING: AtomicBool = AtomicBool::new(false);

impl WindowsKeyboard {
    pub fn new(hwnd: HWND) -> Self {
        Self { hwnd }
    }

    /// ## 重复发送按键
    /// * times: 按键次数
    /// * msg: windows按键消息:WM_KEYDOWN, WM_KEYUP等
    /// * vk: vk码
    fn repeat_send(&self, times: u16, msg: u32, vk: usize) {
        for _ in 0..times {
            unsafe { SendMessageW(self.hwnd, msg, Some(WPARAM(vk)), Some(LPARAM(0isize))) };
            sleep(Duration::from_millis(random_range(50..=100)));
        }
    }

    pub fn press_key_4(&self){
        self.repeat_send(3, WM_KEYDOWN, VK_4.0 as usize );
    }


    pub fn state() -> bool {
        IS_RUNNING.load(Ordering::Relaxed)
    }

    pub fn stop_app() {
        IS_RUNNING.store(false, Ordering::Relaxed);
    }

    pub fn start_app() {
        IS_RUNNING.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::window_util::search_window_by_title;

    #[test]
    fn test_windows_keyboard() {
        if let Some(hwnd) = search_window_by_title("PHANTASY STAR ONLINE 2 NEW GENESIS") {
            let keyboard = WindowsKeyboard::new(hwnd);
            keyboard.press_key_4();
        }
    }
}
