//! 按宿主采集时间归档。查看器与写者共享记录和游标；工作区身份由调用者先行确认。
mod fs;
mod reader;
mod retention;
mod writer;

use chrono::{DateTime, FixedOffset, SecondsFormat, Utc};
pub use fs::new_run_id;
pub use reader::{Catalog, Page, RunInfo, legacy_page, read_page, reveal_path};
use serde::{Deserialize, Serialize};
use std::time::SystemTime;
pub use writer::{RunWriter, Sink};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Stamp {
    pub utc: String,
    pub local: String,
    pub day: String,
    pub offset: i32,
}

impl Stamp {
    pub fn now() -> Self {
        let utc: DateTime<Utc> = SystemTime::now().into();
        let t = utc.timestamp() as libc::time_t;
        let mut tm = std::mem::MaybeUninit::uninit();
        // 使用进程自己的 TZ，与本地菜单、固定偏移验收进程的日界一致。
        let offset = unsafe {
            if libc::localtime_r(&t, tm.as_mut_ptr()).is_null() {
                0
            } else {
                tm.assume_init().tm_gmtoff as i32
            }
        };
        Self::at(utc, offset)
    }
    fn at(utc: DateTime<Utc>, offset: i32) -> Self {
        let local = utc.with_timezone(
            &FixedOffset::east_opt(offset).unwrap_or(FixedOffset::east_opt(0).unwrap()),
        );
        Self {
            utc: utc.to_rfc3339_opts(SecondsFormat::Millis, true),
            local: local.to_rfc3339_opts(SecondsFormat::Millis, false),
            day: local.format("%Y-%m-%d").to_string(),
            offset,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub source: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emulator_run_id: Option<String>,
    #[serde(default)]
    pub continued: bool,
}
impl Event {
    pub fn new(source: &str, text: impl Into<String>) -> Self {
        Self {
            source: source.to_owned(),
            text: text.into(),
            ..Default::default()
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Record {
    pub seq: u64,
    pub segment: u64,
    pub stamp: Stamp,
    #[serde(flatten)]
    event: Event,
}
impl std::ops::Deref for Record {
    type Target = Event;
    fn deref(&self) -> &Event {
        &self.event
    }
}
impl Record {
    fn gap(first: u64, last: u64, stamp: Stamp, segment: u64, reason: &str) -> Self {
        Self {
            seq: last,
            segment,
            stamp,
            event: Event::new(
                "gap",
                format!(
                    "{reason}（序号 {first}–{last}，共 {} 条）",
                    last - first + 1
                ),
            ),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Segment {
    number: u64,
    first: u64,
    last: u64,
    stamp: Stamp,
    latest_utc: String,
    bytes: u64,
    removed: bool,
}
impl Segment {
    fn filename(&self) -> String {
        format!("{:06}-{}.jsonl", self.number, self.stamp.day)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    format: u32,
    source: String,
    id: String,
    instance_id: Option<String>,
    app_run_id: Option<String>,
    started: Stamp,
    ended: Option<Stamp>,
    outcome: Option<String>,
    segments: Vec<Segment>,
    #[serde(default)]
    lost: bool,
}
fn parse_segment(name: &str) -> Option<(u64, &str)> {
    let (n, day) = name.strip_suffix(".jsonl")?.split_once('-')?;
    let num = n.parse::<u64>().ok().filter(|n| *n > 0)?;
    if format!("{num:06}") != n || !valid_day(day) {
        return None;
    }
    Some((num, day))
}
fn valid_day(day: &str) -> bool {
    day.len() == 10 && chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d").is_ok()
}
