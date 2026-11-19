//! 用户 0 的活动任务及最近任务。一个包可以在多台 display 上拥有多个 task。
//! scrcpy 在新 display 中恢复应用后，再从这里读取实际归属。

use std::collections::{BTreeMap, BTreeSet};

use crate::catalog::{checked_shell, require_package, valid_package};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppTask {
    pub package: String,
    pub task_id: u32,
    pub display_id: u32,
    pub resumed: bool,
}

/// 最近记录可以在 Activity 结束后保留，进程也可能仍在缓存中。
/// 列表仅包含活动栈里有 ActivityRecord 的包，包括在后台等待恢复的活动。
pub fn snapshot(device: adb::Device<'_>) -> Result<Vec<AppTask>, String> {
    let out = checked_shell(device, "dumpsys activity activities")?;
    parse_activities(&out.stdout)
}

fn parse_activities(dump: &str) -> Result<Vec<AppTask>, String> {
    // Hist 位于所属 Display 区块。Resumed 行有时是全局汇总，以已见的 taskId 关联。
    // https://android.googlesource.com/platform/frameworks/base/+/android-14.0.0_r75/services/core/java/com/android/server/wm/RootWindowContainer.java
    let mut display = None;
    let mut tasks = BTreeMap::new();
    let mut resumed = BTreeSet::new();
    let mut header = false;
    let mut complete = false;
    for line in dump.lines().map(str::trim) {
        if line.starts_with("ACTIVITY MANAGER ACTIVITIES (dumpsys activity activities)") {
            header = true;
        } else if matches!(
            line,
            "ActivityTaskSupervisor state:" | "ActivityStackSupervisor state:"
        ) {
            // 此节在所有显示历史之后。如果 dumpsys 中途终止，不能据一个空前缀关闭窗口。
            complete = true;
            break;
        } else if let Some(rest) = line.strip_prefix("Display #") {
            display = Some(
                rest.split_ascii_whitespace()
                    .next()
                    .and_then(|id| id.trim_end_matches(':').parse::<u32>().ok())
                    .ok_or("无法读取 Android 显示编号")?,
            );
        } else {
            let hist = line
                .strip_prefix("* ")
                .is_some_and(|s| s.starts_with("Hist "));
            let is_resumed = [
                "Resumed: ",
                "ResumedActivity: ",
                "topResumedActivity=",
                "mResumedActivity: ",
                "mResumedActivity=",
            ]
            .iter()
            .any(|prefix| line.starts_with(prefix));
            if !hist && !is_resumed {
                continue;
            }
            if !hist && !line.contains("ActivityRecord{") {
                continue;
            }
            let record = line
                .split_once("ActivityRecord{")
                .ok_or("无法读取 Android 活动记录")?
                .1;
            let fields: Vec<_> = record.split(['}', ' ']).filter(|s| !s.is_empty()).collect();
            let user = fields
                .get(1)
                .and_then(|v| v.strip_prefix('u'))
                .and_then(|v| v.parse::<u32>().ok())
                .ok_or("活动记录缺少用户编号")?;
            if user != 0
                || fields
                    .iter()
                    .skip(4)
                    .any(|field| matches!(*field, "f" | "isExiting"))
            {
                continue;
            }
            let package = fields
                .get(2)
                .and_then(|component| component.split_once('/'))
                .map(|(pkg, _)| pkg)
                .filter(|pkg| valid_package(pkg))
                .ok_or("活动记录缺少包名")?;
            let id = fields
                .get(3)
                .and_then(|id| id.strip_prefix('t'))
                .and_then(|id| id.parse::<u32>().ok())
                .ok_or("活动记录缺少任务编号")?;
            let key = (package.to_owned(), id);
            if hist {
                let display = display.ok_or("活动没有所属显示")?;
                tasks.insert(key, display);
            } else {
                resumed.insert(key);
            }
        }
    }
    if !header || !complete || display.is_none() {
        return Err("未取得活动栈，无法判断应用是否运行".into());
    }
    Ok(tasks
        .into_iter()
        .map(|(key, display_id)| {
            let resumed = resumed.contains(&key);
            AppTask {
                package: key.0,
                task_id: key.1,
                display_id,
                resumed,
            }
        })
        .collect())
}

/// 每个 block 的身份来自任务自身的组件。顶层活动、启动方和 affinity 都可能指向别的包。
#[derive(Default)]
struct Recent<'a> {
    id: Option<u32>,
    user: Option<u32>,
    owner: Option<&'a str>,
    intent_owner: Option<&'a str>,
}

fn owner_of(component: &str) -> Option<&str> {
    let (pkg, _) = component.trim_matches(['{', '}']).split_once('/')?;
    valid_package(pkg).then_some(pkg)
}

impl<'a> Recent<'a> {
    fn read(&mut self, line: &'a str) -> Result<(), String> {
        if let Some(component) = line
            .strip_prefix("mActivityComponent=")
            .or_else(|| line.strip_prefix("realActivity="))
        {
            self.owner = owner_of(component).or(self.owner);
        } else if line.starts_with("intent={") || line.starts_with("baseIntent=") {
            self.intent_owner = line.split_ascii_whitespace().find_map(|field| {
                field
                    .trim_start_matches("intent={")
                    .strip_prefix("cmp=")
                    .and_then(owner_of)
            });
        } else if let Some(component) = line.strip_prefix("baseActivity=") {
            self.intent_owner = self.intent_owner.or_else(|| owner_of(component));
        }
        if line.starts_with("id=") || line.starts_with("userId=") || line.starts_with("taskId=") {
            for field in line.split_ascii_whitespace() {
                if let Some(id) = field
                    .strip_prefix("taskId=")
                    .or_else(|| field.strip_prefix("id="))
                {
                    Self::confirm(&mut self.id, id)?;
                } else if let Some(user) = field.strip_prefix("userId=") {
                    Self::confirm(&mut self.user, user)?;
                }
            }
        }
        Ok(())
    }

    /// 同一任务标题及详情中的 id 和用户必须一致，冲突时保留该卡片。
    fn confirm(field: &mut Option<u32>, value: &str) -> Result<(), String> {
        let value = value
            .trim_end_matches('}')
            .parse()
            .map_err(|_| "最近任务编号或用户无效")?;
        if field.is_some_and(|old| old != value) {
            return Err("最近任务编号或用户发生冲突".into());
        }
        *field = Some(value);
        Ok(())
    }

    fn collect(self, package: &str, ids: &mut BTreeSet<u32>) -> Result<(), String> {
        let owner = self.owner.or(self.intent_owner);
        if self.user == Some(0) && owner == Some(package) {
            ids.insert(self.id.ok_or("最近任务缺少编号")?);
        } else if self.user.is_none() || (self.user == Some(0) && owner.is_none()) {
            return Err("无法确认最近任务的用户或所属包".into());
        }
        Ok(())
    }
}

fn parse_recents(dump: &str, package: &str) -> Result<BTreeSet<u32>, String> {
    let mut ids = BTreeSet::new();
    let mut recent: Option<Recent<'_>> = None;
    let (mut header, mut evidence, mut raw_list) = (false, false, false);
    for line in dump.lines().map(str::trim) {
        if line.starts_with("ACTIVITY MANAGER RECENT TASKS (dumpsys activity recents)") {
            header = true;
        }
        // 空任务表也会打印 reordering 的最后一个字段。过滤结果为空时输出 (nothing)。
        if line.starts_with("mFreezeTaskListReorderingPendingTimeout=") || line == "(nothing)" {
            evidence = true
        }
        let raw = line.starts_with("* Recent #");
        let info = line.starts_with("* RecentTaskInfo #");
        if raw || info {
            if let Some(previous) = recent.take() {
                previous.collect(package, &mut ids)?;
            }
            // 原始列表含已脱离显示的任务。其后的可见子集采用另一个格式，去重以原始列表为准。
            if info && raw_list {
                break;
            }
            evidence = true;
            raw_list |= raw;
            let mut block = Recent::default();
            if let Some((_, task)) = line.split_once("Task{") {
                for field in task.split_ascii_whitespace() {
                    if let Some(id) = field.strip_prefix('#') {
                        Recent::confirm(&mut block.id, id)?;
                    }
                    if let Some(user) = field.strip_prefix("U=") {
                        Recent::confirm(&mut block.user, user)?;
                    }
                }
            }
            recent = Some(block);
        } else if let Some(block) = &mut recent {
            block.read(line)?;
        }
    }
    if let Some(last) = recent {
        last.collect(package, &mut ids)?;
    }
    if !header || !evidence {
        return Err("未取得最近任务列表".into());
    }
    Ok(ids)
}

fn recent_ids(device: adb::Device<'_>, package: &str) -> Result<BTreeSet<u32>, String> {
    parse_recents(
        &checked_shell(device, &format!("dumpsys activity -p {package} recents"))?.stdout,
        package,
    )
}

fn ensure_inactive(device: adb::Device<'_>, package: &str) -> Result<(), String> {
    if snapshot(device)?.iter().any(|task| task.package == package) {
        return Err(format!("{package} 仍有活动任务"));
    }
    Ok(())
}

/// 清理自退应用的任务卡片。重新核对任务归属及活动栈，应用恢复时中止清理。
pub fn clear_inactive_recents(device: adb::Device<'_>, package: &str) -> Result<(), String> {
    require_package(package)?;
    ensure_inactive(device, package)?;
    let ids = recent_ids(device, package)?;
    if ids.is_empty() {
        return Ok(());
    }
    for id in ids {
        if recent_ids(device, package)?.contains(&id) {
            let active = snapshot(device)?;
            if active
                .iter()
                .any(|task| task.package == package || task.task_id == id)
            {
                return Err(format!("任务 {id} 有活动，保留任务卡片"));
            }
            // API 34 的 runRootTaskRemove 使用 IActivityTaskManager.removeTask(taskId)，
            // 既覆盖已脱离显示的最近任务，也随设备的 AIDL 版本选择正确调用。
            // https://android.googlesource.com/platform/frameworks/base/+/android-14.0.0_r75/services/core/java/com/android/server/am/ActivityManagerShellCommand.java
            checked_shell(device, &format!("am stack remove {id}"))?;
        }
    }
    ensure_inactive(device, package)?;
    if !recent_ids(device, package)?.is_empty() {
        return Err(format!("{package} 仍有最近任务"));
    }
    Ok(())
}

/// 强停后确认包管理器的 stopped 标志，并清理用户 0 仍残留的任务。
pub fn force_stop(device: adb::Device<'_>, package: &str) -> Result<(), String> {
    require_package(package)?;
    checked_shell(device, &format!("am force-stop --user 0 {package}"))?;
    let out = checked_shell(device, &format!("dumpsys package {package}"))?;
    if !stopped(&out.stdout, package) {
        return Err(format!("包管理器未确认 {package} 已停止"));
    }
    clear_inactive_recents(device, package)
}

fn stopped(dump: &str, package: &str) -> bool {
    let expected = format!("Package [{package}]");
    let mut target = false;
    for line in dump.lines().map(str::trim) {
        if line.starts_with("Package [") {
            target = line.split(" (").next() == Some(expected.as_str());
        } else if target && let Some(user) = line.strip_prefix("User 0:") {
            return user
                .split_ascii_whitespace()
                .any(|field| field == "stopped=true")
                && user
                    .split_ascii_whitespace()
                    .any(|field| field == "installed=true");
        }
    }
    false
}
