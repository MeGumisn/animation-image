use flexi_logger::{
    Cleanup, Criterion, DeferredNow, Duplicate, FileSpec, Logger, LoggerHandle, Naming,
};
use log::Record;
use std::thread;

fn my_log_format(
    w: &mut dyn std::io::Write,
    now: &mut DeferredNow,
    record: &Record,
) -> std::io::Result<()> {
    write!(
        w,
        "{} [{:?}] {} [{}] {}",
        // 时间格式：.3f 表示毫秒
        now.format("%Y-%m-%d %H:%M:%S%.3f"),
        // 线程 ID
        thread::current().id(),
        record.level(),
        record.module_path().unwrap_or("<unnamed>"),
        record.args()
    )
}
pub(crate) fn init_logger(level: &str) -> LoggerHandle {
    Logger::try_with_str(level)
        .unwrap()
        .format(my_log_format)
        .log_to_file(
            FileSpec::default()
                .directory("logs") // 日志保存目录
                .basename("auto_press_key_app"), // 文件名前缀
        )
        .rotate(
            Criterion::Age(flexi_logger::Age::Day), // 每天午夜切割
            Naming::Timestamps,                     // 旧文件以时间戳命名
            Cleanup::KeepLogFiles(30),              // 保留最近 30 天的日志
        )
        .duplicate_to_stderr(Duplicate::All)
        .start()
        .unwrap()
}
