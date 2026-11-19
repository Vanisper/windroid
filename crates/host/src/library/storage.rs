//! 预览保留在宿主。确认只引用上一次预览的编号，文件与目录归属由运行时重新核对。
use super::*;
use runtime::storage::{self, MigrationPlan, StorageLocations};
use std::path::Path;

pub(super) struct Resources {
    pub provisioner: Provisioner,
    pub error: Option<String>,
    paths: RuntimePaths,
    inherited: bool,
    preview: Option<(u64, MigrationPlan)>,
}

impl Resources {
    pub fn open(home: &Path) -> Self {
        let config = storage::recover(home).and_then(|_| StorageLocations::load(home));
        let provisioner = Provisioner::new(home);
        Self {
            paths: provisioner.paths(),
            provisioner,
            error: config
                .as_ref()
                .err()
                .map(|e| format!("读取或恢复存储位置失败，请核对目录后重新检查：{e}")),
            inherited: config.is_ok_and(|value| value.image_root.is_none()),
            preview: None,
        }
    }

    pub fn preparation(&self) -> JsonValue {
        let mut view = crate::to_json(&self.provisioner.snapshot());
        if let Some(error) = &self.error {
            view["error"] = json!(error);
            view["componentsReady"] = false.into();
            view["phase"] = "failed".into();
        }
        view
    }

    pub fn locations(&self, home: &Path) -> JsonValue {
        let paths = &self.paths;
        json!({"home":home, "sdk":paths.sdk, "runtimeRoot":paths.runtime_root, "images":paths.images,
               "imagesInherited": self.inherited, "borrowed": paths.borrowed, "error": self.error})
    }
}

fn transfer_uses(plan: &MigrationPlan, p: &Provisioner) -> bool {
    let (components, images) = plan.changed_roots();
    let snapshot = p.snapshot();
    let active = matches!(snapshot.phase, "downloading" | "installing" | "verifying");
    let preparing_image = snapshot
        .images
        .iter()
        .any(|row| Some(row.id) == snapshot.current_component);
    active && if preparing_image { images } else { components }
}

impl Library {
    pub(super) fn reload_resources(&self) -> Result<(), String> {
        let runtime = self.runtime.as_ref().ok_or("演示环境已就绪")?;
        let provisioner = runtime.lock().unwrap().provisioner.clone();
        let config = storage::recover(&self.layout.root)
            .and_then(|_| StorageLocations::load(&self.layout.root))
            .map_err(|e| e.to_string());
        provisioner.refresh_locations();
        let paths = provisioner.paths();
        let mut resources = runtime.lock().unwrap();
        resources.paths = paths;
        resources.preview = None;
        resources.inherited = config
            .as_ref()
            .is_ok_and(|config| config.image_root.is_none());
        resources.error = config.as_ref().err().cloned();
        config.map(|_| ())
    }

    pub(super) fn connect_storage(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        self.channel.handle("windroid.previewStorage", move |call| {
            let Some(lib) = weak.upgrade() else {
                return call.reject("应用库已关闭");
            };
            let requested = call.data().clone();
            thread::spawn(move || call.reply(lib.preview_storage(&requested)));
        });
    }

    fn preview_storage(&self, data: &JsonValue) -> Result<JsonValue, String> {
        let requested: StorageLocations =
            serde_json::from_value(data.clone()).map_err(|e| e.to_string())?;
        let expected: StorageLocations = serde_json::from_value(data["expected"].clone())
            .map_err(|_| "请重新读取当前存储位置")?;
        let runtime = self.runtime.as_ref().ok_or("演示环境没有受管目录")?;
        if runtime.lock().unwrap().paths.borrowed {
            return Err("开发者指定的 SDK 由外部管理".into());
        }
        // 窗口打开期间，另一个进程可能已移动用户并未要求修改的目录。
        // 预览前后均检查，确认时运行时还会复核实际清单与起点。
        let stale = || {
            let _ = self.reload_resources();
            self.notify();
            "存储位置已变化，请按最新位置重新选择目标目录".to_owned()
        };
        if StorageLocations::load(&self.layout.root).map_err(|e| e.to_string())? != expected {
            return Err(stale());
        }
        let plan = storage::preview(&self.layout.root, &requested).map_err(|e| e.to_string())?;
        if plan.before != expected {
            return Err(stale());
        }
        let mut blockers: Vec<String> = self
            .state
            .lock()
            .unwrap()
            .instances
            .values()
            .filter(|i| {
                plan.changed_roots() != (false, false) && i.handle.snapshot().facts.process_alive
            })
            .map(|i| format!("请先停止「{}」", i.name))
            .collect();
        let id = self.revision();
        let mut resources = runtime.lock().unwrap();
        if transfer_uses(&plan, &resources.provisioner) {
            blockers.push("请等待当前下载或安装完成".into());
        }
        if let Some(failure) = &resources.error {
            blockers.push(failure.clone());
        }
        let result = json!({"previewId":id, "runtimeRoot":plan.after.runtime_root, "imageRoot":plan.after.image_root,
            "files":plan.files, "blockers":blockers});
        resources.preview = Some((id, plan));
        Ok(result)
    }

    pub(super) fn apply_storage(&self, data: &JsonValue) -> Reply {
        let id = data["plan"]["previewId"]
            .as_u64()
            .ok_or_else(|| error("请先预览存储位置"))?;
        let runtime = self
            .runtime
            .as_ref()
            .ok_or_else(|| error("演示环境没有受管目录"))?;
        let _admission = self.starting.lock().unwrap();
        let plan = {
            let resources = runtime.lock().unwrap();
            let (_, plan) = resources
                .preview
                .as_ref()
                .filter(|(preview, _)| *preview == id)
                .ok_or_else(|| error("预览已过期，请重新查看迁移文件"))?;
            if transfer_uses(plan, &resources.provisioner) {
                return Err(error("请等待相关下载或安装完成"));
            }
            plan.clone()
        };
        let result = storage::apply(&self.layout.root, &plan);
        // 提交后如旧卷失联，设置可能已经切换。以事务恢复后的配置重建自检快照。
        let reload = self.reload_resources();
        result.map_err(error)?;
        reload.map_err(error)?;
        self.note(None, false, "存储位置已更新");
        Ok(self.revision())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stale_window_cannot_move_an_unselected_root_back() {
        let dir = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("windroid-stale-storage-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let home = dir.join("home");
        let (windows, _) = mpsc::channel();
        let library = Library::new(
            HostChannel::new(),
            Layout { root: home.clone() },
            false,
            windows,
            false,
        );
        let expected = StorageLocations::defaults(&home);
        let new = StorageLocations {
            runtime_root: dir.join("components"),
            image_root: None,
        };
        storage::apply(&home, &storage::preview(&home, &new).unwrap()).unwrap();
        let request = json!({"runtimeRoot":expected.runtime_root, "imageRoot":dir.join("images"), "expected":expected});
        assert!(
            library
                .preview_storage(&request)
                .unwrap_err()
                .contains("重新选择")
        );
        assert_eq!(
            library.snapshot()["storage"]["runtimeRoot"],
            json!(new.runtime_root)
        );
        assert_eq!(StorageLocations::load(&home).unwrap(), new);
        let request =
            json!({"runtimeRoot":new.runtime_root, "imageRoot":dir.join("images"), "expected":new});
        let plan = library.preview_storage(&request).unwrap();
        library.apply_storage(&json!({"plan":plan})).unwrap();
        let installed = StorageLocations::load(&home).unwrap();
        assert_eq!(installed.runtime_root, new.runtime_root);
        assert_eq!(installed.image_root, Some(dir.join("images")));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
