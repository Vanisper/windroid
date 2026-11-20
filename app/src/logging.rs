//! 保留 RUST_LOG 对终端的过滤，应用自身的持久诊断独立于开发者的终端设置。
use log::{Level, LevelFilter, Log, Metadata, Record};

struct Logger(env_logger::Logger);

impl Log for Logger {
    fn enabled(&self, meta: &Metadata<'_>) -> bool {
        meta.level() <= Level::Warn || self.0.enabled(meta)
    }

    fn log(&self, record: &Record<'_>) {
        runtime::app_log::warning(record);
        self.0.log(record);
    }

    fn flush(&self) {
        self.0.flush();
    }
}

pub fn install() {
    let terminal =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).build();
    let max = terminal.filter().max(LevelFilter::Warn);
    if log::set_boxed_logger(Box::new(Logger(terminal))).is_ok() {
        log::set_max_level(max);
    }
}
