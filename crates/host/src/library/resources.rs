//! 资源的下载、导入与创建实例分别确认。大文件检查在工作线程执行，快照仅读取缓存。

use super::*;
use std::path::Path;

pub(super) fn demo_image() -> contract::Image {
    contract::Image {
        revision: 14,
        api: 34,
        tag: "google_apis_playstore".into(),
        abi: "arm64-v8a".into(),
    }
}

pub(super) fn image_available(preparation: &JsonValue, wanted: &contract::Image) -> bool {
    preparation["images"].as_array().is_some_and(|rows| {
        rows.iter().any(|row| {
            let image = &row["image"];
            row["ready"] == true
                && image["revision"] == wanted.revision
                && image["api"] == wanted.api
                && image["abi"] == wanted.abi
                && image["tag"] == wanted.tag
        })
    })
}

impl Library {
    pub(super) fn connect_resources(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        self.channel.handle("windroid.inspectImage", move |call| {
            let Some(lib) = weak.upgrade() else {
                return call.reject("应用库已关闭");
            };
            thread::spawn(move || {
                let result = (|| {
                    let path = call.data()["path"].as_str().ok_or("请选择官方镜像 ZIP")?;
                    let provisioner = lib
                        .runtime
                        .as_ref()
                        .ok_or("演示环境不支持导入")?
                        .lock()
                        .unwrap()
                        .provisioner
                        .clone();
                    let component = provisioner.inspect_image_zip(Path::new(path))?;
                    Ok(json!({"imageId":component.id}))
                })();
                call.reply(result);
            });
        });
    }

    pub(super) fn preparation(&self) -> JsonValue {
        if let Some(p) = &self.runtime {
            return p.lock().unwrap().preparation();
        }
        json!({
            "phase":"ready", "componentsReady":true, "components":[],
            "images":[{"id":"system-image","name":"Android 14 Google Play","version":"14",
                "image":demo_image(),"ready":true,"location":"内存演示环境","integrity":"verified","repairToken":null,"writable":false,"requiredBytes":0,"availableBytes":null,"issue":null,
                "sizeBytes":0,"downloadedBytes":0,"source":"","license":"演示资源","licenseUrl":""}],
            "currentComponent":null,"candidateTarget":null,"candidateDownloadedBytes":null, "downloadedBytes":0, "totalBytes":0, "error":null
        })
    }

    pub(super) fn prepare(&self, kind: &str, data: &JsonValue) -> Reply {
        let p = self
            .runtime
            .as_ref()
            .ok_or_else(|| error("演示环境已就绪"))?;
        if kind == "recheckRuntime" {
            self.reload_resources().map_err(error)?;
            return Ok(self.revision());
        }
        let p = p.lock().unwrap();
        if let Some(failure) = &p.error {
            return Err(error(failure));
        }
        let p = &p.provisioner;
        let image_id = || {
            data["imageId"]
                .as_str()
                .ok_or_else(|| error("请选择系统镜像版本"))
        };
        if matches!(
            kind,
            "prepareRuntime" | "prepareImage" | "importImage" | "repairResource"
        ) && data["acceptedLicenses"] != true
        {
            return Err(error("请先阅读并接受所选资源的许可"));
        }
        match kind {
            "prepareRuntime" => p.start(),
            "prepareImage" => p.start_image(image_id()?),
            "importImage" => {
                let path = data["path"]
                    .as_str()
                    .ok_or_else(|| error("请选择镜像 ZIP"))?;
                p.import_image(image_id()?, path.into())
            }
            "verifyResource" => p.verify(
                data["resourceId"]
                    .as_str()
                    .ok_or_else(|| error("请选择要核验的版本"))?,
            ),
            "repairResource" => {
                let id = data["resourceId"]
                    .as_str()
                    .ok_or_else(|| error("请选择要修复的版本"))?;
                let snapshot = p.snapshot();
                let row = snapshot
                    .components
                    .iter()
                    .chain(&snapshot.images)
                    .find(|r| r.id == id)
                    .ok_or_else(|| error("版本已不在发布清单中"))?;
                if data["confirmed"] != true || data["location"] != json!(row.location) {
                    return Err(error("资源位置已变化或尚未确认，请重新确认此版本及路径"));
                }
                let token = data["repairToken"]
                    .as_str()
                    .filter(|token| !token.is_empty())
                    .ok_or_else(|| error("请重新检查并确认要修复的版本"))?;
                if row.integrity != "damaged" || row.repair_token.as_deref() != Some(token) {
                    return Err(error("目标卷或版本状态已变化，请重新确认"));
                }
                p.repair_confirmed(id, &row.location, token)
            }
            "pausePreparation" => p.pause(),
            _ => unreachable!("unknown resource action"),
        }
        .map_err(error)?;
        Ok(self.revision())
    }

    pub(super) fn selected_image(&self, data: &JsonValue) -> Result<contract::Image, Rejection> {
        let id = data["imageId"]
            .as_str()
            .ok_or_else(|| error("请选择创建实例使用的系统镜像"))?;
        let requested: contract::Image = serde_json::from_value(data["image"].clone())
            .map_err(|_| error("创建实例必须指定镜像 API、标签、ABI 和修订号 revision"))?;
        let validate = |image: contract::Image| {
            if image == requested {
                Ok(image)
            } else {
                Err(error("所选镜像身份（含修订号）与发布清单不一致"))
            }
        };
        let Some(p) = &self.runtime else {
            return if id == "system-image" {
                validate(demo_image())
            } else {
                Err(error("演示环境中没有该镜像"))
            };
        };
        let row = p
            .lock()
            .unwrap()
            .provisioner
            .snapshot()
            .images
            .into_iter()
            .find(|row| row.id == id)
            .ok_or_else(|| error("系统镜像不在发布清单中"))?;
        if !row.ready {
            return Err(error("请先完成所选镜像的下载或导入"));
        }
        validate(row.image.ok_or_else(|| error("镜像清单缺少实例规格"))?)
    }
}
