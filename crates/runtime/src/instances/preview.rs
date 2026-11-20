//! 对固定的启动目标逐台检查，只在内存中累计可启动实例的资源需求。

use super::{InstanceStore, check_ports, image_subdir, invalid, process, quick_boot, read_record};
use crate::ActiveRuntime;
use contract::InstanceId;
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

impl InstanceStore {
    /// ids 由调用方按列表顺序提供，并已排除当前 actor 状态不允许启动的实例。
    /// 每个 ID 对应一个结果。预览无预留效力，真正执行仍通过该实例 actor 的 Start。
    pub fn preview_starts(
        &self,
        ids: &[InstanceId],
        runtime: &ActiveRuntime,
    ) -> Vec<io::Result<()>> {
        // 同一批里相同修订只核验一次，避免首次缺少缓存时反复读取完整镜像。
        let mut versions = BTreeMap::new();
        let candidates: Vec<io::Result<_>> = ids
            .iter()
            .map(|id| {
                let root = self.home().canonicalize()?.join("instances");
                let record = read_record(&root, id).map_err(|e| {
                    if e.kind() == io::ErrorKind::NotFound {
                        invalid("实例记录已不存在")
                    } else {
                        e
                    }
                })?;
                let dir = root.join(id.as_str());
                check_run_lock(&dir)?;
                process::check_previous_exit(&dir)?;
                check_ports(record.port)?;
                let (paths, _leases) = versions
                    .entry(image_subdir(&record.spec.image)?)
                    .or_insert_with(|| (runtime.preview)(self.home(), &record.spec.image))
                    .as_ref()
                    .map_err(|e| io::Error::new(e.kind(), e.to_string()))?;
                process::launch_paths(&dir, &record, paths)?;
                quick_boot::requires_cold_start(&dir, &record)?;
                let (_, demand, budget) = process::launch_requirements(self, &record, paths)?;
                Ok((record.port, demand, budget))
            })
            .collect();

        // 大包核验可能耗时。候选项检查结束后再采样，所有模拟申请共用这份观测。
        let mut preview = None;
        candidates
            .into_iter()
            .map(|candidate| {
                let (port, demand, budget) = candidate?;
                preview
                    .get_or_insert_with(|| runtime.admission.preview(self))
                    .as_mut()
                    .map_err(|e| io::Error::new(e.kind(), e.to_string()))?
                    .include(port, demand, budget)
            })
            .collect()
    }
}

// 某个客户端可能刚取得运行锁、尚未发布 PID 或绑定端口。未启动过的实例没有 run.lock。
fn check_run_lock(dir: &Path) -> io::Result<()> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(dir.join("run.lock"))
    {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        other => other?,
    };
    if !file.metadata()?.is_file() {
        return Err(invalid("实例运行锁需要普通文件"));
    }
    file.try_lock_shared().map_err(|e| {
        let err = io::Error::from(e);
        io::Error::new(err.kind(), format!("实例运行锁被占用或无法读取：{err}"))
    })?;
    file.unlock()
}
