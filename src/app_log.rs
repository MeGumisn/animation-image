//! 日志：直接用 flexi_logger（按天切割、保留 30 天），并记录异常退出。

use flexi_logger::{
    Age, Cleanup, Criterion, DeferredNow, Duplicate, FileSpec, Logger, LoggerHandle, Naming,
};
use log::Record;
use std::path::{Path, PathBuf};
use std::sync::Once;
use std::{env, fs};

/// 日志目录（相对当前工作目录）
const LOG_DIR: &str = "logs";
/// 文件名前缀 → logs/animegan_rCURRENT.log
const LOG_BASENAME: &str = "animegan";
/// 运行期间存在、正常退出时删除；下次启动还在就说明上次是崩溃/被强杀
const RUN_MARKER: &str = ".last_run";

fn my_log_format(
    w: &mut dyn std::io::Write,
    now: &mut DeferredNow,
    record: &Record,
) -> std::io::Result<()> {
    write!(
        w,
        "{} {:<5} {} - {}",
        // 2026-10-20 03:50:17.123
        now.format("%Y-%m-%d %H:%M:%S%.3f"),
        record.level(),
        // 模块路径，比如 animation_imagee::gpu_window
        record.target(),
        record.args()
    )
}

pub(crate) fn init_logger(level: &str) -> LoggerHandle {
    unsafe { env::set_var("HF_ENDPOINT", "https://hf-mirror.com") };
    Logger::try_with_str(level)
        .expect("日志级别配置有误")
        .format(my_log_format)
        .log_to_file(
            FileSpec::default()
                .directory(LOG_DIR)
                .basename(LOG_BASENAME)
                // 当前文件固定叫 animegan_rCURRENT.log，只有切割出来的旧文件带时间戳
                .suppress_timestamp(),
        )
        // 重启后接着往同一个文件写，别把上一次的日志冲掉
        .append()
        .rotate(
            Criterion::Age(Age::Day),       // 每天午夜切割
            Naming::Timestamps,             // 旧文件以时间戳命名
            Cleanup::KeepLogFiles(30),      // 保留最近 30 个文件
        )
        .duplicate_to_stderr(Duplicate::All)
        .start()
        .expect("启动日志失败")
}

fn marker_path() -> PathBuf {
    Path::new(LOG_DIR).join(RUN_MARKER)
}

fn write_run_marker() {
    let mut now = DeferredNow::new();
    let stamp = format!(
        "{} pid={}",
        now.format("%Y-%m-%d %H:%M:%S"),
        std::process::id()
    );
    let _ = fs::write(marker_path(), stamp);
}

/// 上次是否有没清掉的运行标记（崩溃、被任务管理器结束、panic=abort 都会留下）
fn report_last_run() {
    if let Ok(prev) = fs::read_to_string(marker_path()) {
        let prev = prev.trim();
        if !prev.is_empty() {
            log::warn!("上次运行没有正常退出（启动于 {prev}）：崩溃或被强制结束，可能留下了上屏窗口");
        }
    }
    write_run_marker();
}

/// panic 也要留痕：release 里 panic = "abort"，进程会立刻结束，必须先把日志刷盘
fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("<unnamed>");
        let location = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_default();
        log::error!("程序异常退出（panic）：{info} | 线程={name} | 位置={location}");
        log::logger().flush();
        default_hook(info);
    }));
}

pub fn init() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let handle = init_logger("info");
        // 这个 handle 一旦被 drop，flexi_logger 的后台写盘线程就停了，所以留到进程结束
        std::mem::forget(handle);
        install_panic_hook();
        log::info!(
            "==== 启动 === 日志目录 {LOG_DIR}（当前文件 {LOG_BASENAME}_rCURRENT.log），进程 {}",
            std::process::id()
        );
        report_last_run();
    });
}

/// 正常退出时调用（由 main 里的守卫对象触发）：清掉运行标记并刷盘。
/// panic / 崩溃 / 强杀时不会走到这里，标记会留下，下次启动就能发现异常退出。
pub fn mark_clean_exit() {
    let _ = fs::remove_file(marker_path());
    log::logger().flush();
}
