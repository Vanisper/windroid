use super::catalog::{Artifact, EMULATOR_VERSION, IMAGE_ABI, PLAY_TAG};
use super::integrity::invalid;
use std::io;

fn revision(value: &str) -> Option<[u32; 3]> {
    let mut parts = value.split('.');
    let mut result = [0; 3];
    for field in &mut result {
        if let Some(part) = parts.next() {
            *field = part.parse().ok()?;
        }
    }
    parts.next().is_none().then_some(result)
}

/// 已安装目录与官方 ZIP 预览共用包身份、修订和兼容要求校验。
pub(super) fn verify(props: &str, pkg: Artifact) -> io::Result<()> {
    let property = |key: &str| {
        props
            .lines()
            .filter_map(|s| s.trim().split_once('='))
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.trim())
    };
    if property("Pkg.Revision") != Some(pkg.version) {
        return Err(invalid(format!(
            "{} 需要版本 {}，实际为 {}",
            pkg.name,
            pkg.version,
            property("Pkg.Revision").unwrap_or("未知")
        )));
    }
    if let Some(image) = pkg.image_spec {
        let matches_package = property("AndroidVersion.ApiLevel").and_then(|api| api.parse().ok())
            == Some(image.api)
            && property("SystemImage.TagId") == Some(PLAY_TAG)
            && property("SystemImage.Abi") == Some(IMAGE_ABI)
            && property("Addon.VendorId") == Some("google");
        if !matches_package {
            return Err(invalid("系统镜像的发布者、API、标签或 ABI 与清单不符"));
        }
        // source.properties 声明的依赖必须与官方 XML 的最低 Emulator 版本相符。
        let dependency = property("Pkg.Dependencies").and_then(|v| v.strip_prefix("emulator#"));
        let required = revision(image.min_emulator);
        if dependency.and_then(revision) != required
            || !required
                .is_some_and(|v| v <= revision(EMULATOR_VERSION).expect("pinned emulator revision"))
        {
            return Err(invalid("镜像声明的最低模拟器版本不兼容或与清单不符"));
        }
    }
    Ok(())
}
