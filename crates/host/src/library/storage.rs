//! 切换资源目录前由宿主保留完整预览。确认和候选目录下载只接受上次预览编号。
use super::*;
use runtime::storage::{self, MigrationPlan, StorageLocations, SwitchMode};
use std::path::Path;

pub(super) struct Resources {
    pub provisioner: Provisioner,
    pub error: Option<String>,
    paths: RuntimePaths,
    cleanup: Vec<String>,
    preview: Option<(u64, MigrationPlan)>,
}

impl Resources {
    /// 调用者已经只读确认这是可打开的新格式工作区。
    pub fn open(home: &Path) -> Self {
        let result = storage::recover(home).and_then(|_| StorageLocations::load(home));
        let provisioner = Provisioner::new(home);
        Self {
            paths: provisioner.paths(),
            provisioner,
            error: result.err().map(|e| e.to_string()),
            cleanup: storage::pending_cleanup(home).unwrap_or_default(),
            preview: None,
        }
    }

    pub fn preparation(&self) -> JsonValue {
        let mut view = crate::to_json(&self.provisioner.snapshot());
        if let Some(error) = &self.error {
            view["error"] = json!(error);
            view["phase"] = "failed".into();
            view["componentsReady"] = false.into();
        }
        view
    }

    pub fn locations(&self, home: &Path) -> JsonValue {
        let defaults = StorageLocations::defaults(home);
        json!({"home":home,"runtimeRoot":self.paths.runtime_root,"imageRoot":self.paths.images,
               "defaultRuntimeRoot":defaults.runtime_root,"defaultImageRoot":defaults.image_root,
               "error":self.error,"cleanupNotes":self.cleanup})
    }
}

fn transfer_uses(plan: &MigrationPlan, p: &Provisioner) -> bool {
    let (cores, images) = plan.changed_roots();
    let snapshot = p.snapshot();
    let active = matches!(snapshot.phase, "downloading" | "installing" | "verifying");
    let image = snapshot
        .images
        .iter()
        .any(|row| Some(row.id) == snapshot.current_component);
    active && if image { images } else { cores }
}

impl Library {
    pub(super) fn reload_resources(&self) -> Result<(), String> {
        let runtime = self.runtime.as_ref().ok_or("演示环境已就绪")?;
        let p = runtime.lock().unwrap().provisioner.clone();
        let result = storage::recover(&self.layout.root)
            .and_then(|_| StorageLocations::load(&self.layout.root))
            .and_then(|_| storage::pending_cleanup(&self.layout.root))
            .map_err(|e| e.to_string());
        p.refresh_locations();
        let mut res = runtime.lock().unwrap();
        res.preview = None;
        res.paths = p.paths();
        res.error = result.as_ref().err().cloned();
        result.map(|notes| res.cleanup = notes)
    }

    pub(super) fn connect_storage(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        self.channel.handle("windroid.previewStorage", move |call| {
            let Some(lib) = weak.upgrade() else {
                return call.reject("应用库已关闭");
            };
            let data = call.data().clone();
            thread::spawn(move || call.reply(lib.preview_storage(&data)));
        });
    }

    fn preview_storage(&self, data: &JsonValue) -> Result<JsonValue, String> {
        let requested: StorageLocations =
            serde_json::from_value(data.clone()).map_err(|e| e.to_string())?;
        let expected: StorageLocations = serde_json::from_value(data["expected"].clone())
            .map_err(|_| "请重新读取当前存储位置")?;
        let mode: SwitchMode =
            serde_json::from_value(data["mode"].clone()).map_err(|_| "请选择切换方式")?;
        let cleanup = data["cleanup"].as_bool().ok_or("请选择是否清理原版本")?;
        let runtime = self.runtime.as_ref().ok_or("演示环境没有资源目录")?;
        let stale = || {
            let _ = self.reload_resources();
            "存储位置已更改，请重新选择目标目录".to_owned()
        };
        if StorageLocations::load(&self.layout.root).map_err(|e| e.to_string())? != expected {
            return Err(stale());
        }
        let mut plan = storage::preview_switch(&self.layout.root, &requested, mode, cleanup)
            .map_err(|e| e.to_string())?;
        if plan.before != expected {
            return Err(stale());
        }
        let id = self.revision();
        let mut resources = runtime.lock().unwrap();
        if transfer_uses(&plan, &resources.provisioner) {
            plan.blockers.push("请等待相关下载或安装完成".into());
        }
        let mut result = crate::to_json(&plan);
        result["runtimeRoot"] = json!(plan.after.runtime_root);
        result["imageRoot"] = json!(plan.after.image_root);
        result["previewId"] = json!(id);
        resources.preview = Some((id, plan));
        Ok(result)
    }

    pub(super) fn prepare_candidate_image(&self, data: &JsonValue) -> Reply {
        if data["acceptedLicenses"] != true {
            return Err(error("请先阅读并同意所选镜像许可"));
        }
        let id = data["previewId"]
            .as_u64()
            .ok_or_else(|| error("请先预览目标目录"))?;
        let package = data["resourceId"]
            .as_str()
            .ok_or_else(|| error("请选择所缺镜像"))?;
        let runtime = self
            .runtime
            .as_ref()
            .ok_or_else(|| error("演示环境没有资源目录"))?;
        let resources = runtime.lock().unwrap();
        let plan = &resources
            .preview
            .as_ref()
            .filter(|(selected, _)| *selected == id)
            .ok_or_else(|| error("预览已过期，请重新核验"))?
            .1;
        if plan.before != StorageLocations::load(&self.layout.root).map_err(error)?
            || !plan
                .versions
                .iter()
                .any(|v| v.id == package && v.downloadable)
        {
            return Err(error("请重新预览并选择目标目录所缺的镜像"));
        }
        resources
            .provisioner
            .start_image_at(package, &plan.after)
            .map_err(error)?;
        Ok(self.revision())
    }

    pub(super) fn apply_storage(&self, data: &JsonValue) -> Reply {
        let id = data["plan"]["previewId"]
            .as_u64()
            .ok_or_else(|| error("请先预览存储位置"))?;
        let runtime = self
            .runtime
            .as_ref()
            .ok_or_else(|| error("演示环境没有资源目录"))?;
        let _admission = self.starting.lock().unwrap();
        let plan = {
            let resources = runtime.lock().unwrap();
            let (_, plan) = resources
                .preview
                .as_ref()
                .filter(|(selected, _)| *selected == id)
                .ok_or_else(|| error("预览已过期，请重新核验目录"))?;
            if transfer_uses(plan, &resources.provisioner) {
                return Err(error("请等待相关资源准备完成"));
            }
            plan.clone()
        };
        let result = storage::apply(&self.layout.root, &plan);
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
    fn stale_preview_cannot_restore_a_different_resource_root() {
        let temp = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("windroid-storage-host-{}", std::process::id()));
        std::fs::create_dir(&temp).unwrap();
        let home = temp.join("home");
        let (windows, _) = mpsc::channel();
        let library = Library::new(
            HostChannel::new(),
            Layout { root: home.clone() },
            false,
            windows,
            false,
        );
        let defaults = StorageLocations::defaults(&home);
        let new = StorageLocations {
            runtime_root: temp.join("tools"),
            ..defaults.clone()
        };
        let plan = storage::preview_switch(&home, &new, SwitchMode::Future, false).unwrap();
        storage::apply(&home, &plan).unwrap();
        let request = json!({"runtimeRoot":defaults.runtime_root,"imageRoot":temp.join("images"),"mode":"future","cleanup":false,"expected":defaults});
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
        let request = json!({"runtimeRoot":new.runtime_root,"imageRoot":temp.join("images"),"mode":"future","cleanup":false,"expected":new});
        let plan = library.preview_storage(&request).unwrap();
        library.apply_storage(&json!({"plan":plan})).unwrap();
        let saved = StorageLocations::load(&home).unwrap();
        assert_eq!(saved.runtime_root, new.runtime_root);
        assert_eq!(saved.image_root, temp.join("images"));
        std::fs::remove_dir_all(temp).unwrap();
    }
}
