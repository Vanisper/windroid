//! 预览仅识别固定清单中的完整官方包。确认后先复制到不带目录项的私有文件，
//! 安装读取与校验相同的字节，即使用户移动或覆盖最初选择的 ZIP。
use super::catalog::Artifact;
use super::integrity::{check_file, invalid};
use super::{Shared, metadata, paths};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

pub(super) fn inspect(path: &Path, catalog: &[Artifact]) -> io::Result<Artifact> {
    let bytes = paths::regular(path)?.len();
    let mut candidates = catalog
        .iter()
        .copied()
        .filter(|pkg| pkg.image_spec.is_some() && pkg.bytes == bytes)
        .peekable();
    if candidates.peek().is_none() {
        return Err(invalid(
            "文件大小与清单中的官方完整镜像 ZIP 不符，请选择受支持的版本",
        ));
    }
    let file = File::open(path)?;
    for pkg in candidates {
        match check_file(&file, pkg, || Ok(())) {
            Ok(()) => (),
            Err(e) if e.kind() == io::ErrorKind::InvalidData => continue,
            Err(e) => return Err(e),
        }
        let mut zip = zip::ZipArchive::new(&file)?;
        let name = format!(
            "{}/source.properties",
            pkg.prefix.expect("catalog image ZIP")
        );
        let source = zip.by_name(&name)?;
        if source.size() > 16_384 {
            return Err(invalid("镜像元数据过大"));
        }
        let mut props = String::new();
        source.take(16_385).read_to_string(&mut props)?;
        metadata::verify(&props, pkg)?;
        return Ok(pkg);
    }
    Err(invalid(
        "镜像校验和与官方清单不符，文件可能不完整或已被修改",
    ))
}

pub(super) fn capture(shared: &Shared, source: &Path, pkg: Artifact) -> io::Result<File> {
    if paths::regular(source)?.len() != pkg.bytes {
        return Err(invalid("镜像 ZIP 在确认后发生了变化，请重新选择"));
    }
    let mut source = File::open(source)?.take(pkg.bytes + 1);
    let temp = shared
        .stage
        .join(format!("image-import-{}", crate::instances::nonce()?));
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    fs::remove_file(temp)?;
    let mut copied = 0;
    let mut buffer = vec![0; 256 * 1024];
    loop {
        shared.check_pause()?;
        let count = source.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        copied += count as u64;
        if copied > pkg.bytes {
            return Err(invalid("镜像 ZIP 在确认后发生了变化，请重新选择"));
        }
        file.write_all(&buffer[..count])?;
    }
    Ok(file)
}
