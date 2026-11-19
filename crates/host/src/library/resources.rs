//! 资源的下载、导入与创建实例分别确认。大文件检查在工作线程执行，快照仅读取缓存。

use super::*;
use std::path::Path;

fn demo_image() -> contract::Image {
    contract::Image {
        api: 34,
        tag: "google_apis_playstore".into(),
        abi: "arm64-v8a".into(),
    }
}

pub(super) fn image_available(preparation: &JsonValue, wanted: &contract::Image) -> bool {
    // 开发者 SDK 可保留旧实例原有的其他镜像，启动时按记录校验实际路径。
    if preparation["borrowed"] == true {
        return true;
    }
    let abi = if wanted.abi == "arm64" {
        "arm64-v8a"
    } else {
        &wanted.abi
    };
    preparation["images"].as_array().is_some_and(|rows| {
        rows.iter().any(|row| {
            let image = &row["image"];
            row["ready"] == true
                && image["api"] == wanted.api
                && image["abi"] == abi
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
                    let provisioner = lib.runtime.as_ref().ok_or("演示环境不支持导入")?.clone();
                    let component = provisioner.inspect_image_zip(Path::new(path))?;
                    Ok(json!({"imageId":component.id}))
                })();
                call.reply(result);
            });
        });
    }

    pub(super) fn preparation(&self) -> JsonValue {
        if let Some(p) = &self.runtime {
            return crate::to_json(&p.snapshot());
        }
        json!({
            "phase":"ready", "componentsReady":true, "borrowed":false, "components":[],
            "images":[{"id":"system-image","name":"Android 14 Google Play","version":"14",
                "image":demo_image(),"ready":true,"borrowed":false,"location":"内存演示环境",
                "sizeBytes":0,"downloadedBytes":0,"source":"","license":"演示资源","licenseUrl":""}],
            "currentComponent":null, "downloadedBytes":0, "totalBytes":0, "error":null
        })
    }

    pub(super) fn prepare(&self, kind: &str, data: &JsonValue) -> Reply {
        let p = self
            .runtime
            .as_ref()
            .ok_or_else(|| error("演示环境已就绪"))?;
        let image_id = || {
            data["imageId"]
                .as_str()
                .ok_or_else(|| error("请选择系统镜像版本"))
        };
        if matches!(kind, "prepareRuntime" | "prepareImage" | "importImage")
            && data["acceptedLicenses"] != true
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
            "pausePreparation" => p.pause(),
            _ => p.recheck(),
        }
        .map_err(error)?;
        Ok(self.revision())
    }

    pub(super) fn selected_image(&self, data: &JsonValue) -> Result<contract::Image, Rejection> {
        let id = data["imageId"]
            .as_str()
            .ok_or_else(|| error("请选择创建实例使用的系统镜像"))?;
        let Some(p) = &self.runtime else {
            return (id == "system-image")
                .then(demo_image)
                .ok_or_else(|| error("演示环境中没有该镜像"));
        };
        let row = p
            .snapshot()
            .images
            .into_iter()
            .find(|row| row.id == id)
            .ok_or_else(|| error("系统镜像不在发布清单中"))?;
        if !row.ready {
            return Err(error("请先完成所选镜像的下载或导入"));
        }
        row.image.ok_or_else(|| error("镜像清单缺少实例规格"))
    }
}
