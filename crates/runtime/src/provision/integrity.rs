use std::fs::File;
use std::io::{self, Read, Seek};
use std::path::Path;

use sha2::Digest as Hasher;

use super::catalog::{Artifact, Digest};
use super::paths::regular;

pub(super) fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

pub(super) fn check_digest(
    path: &Path,
    pkg: Artifact,
    check: impl Fn() -> io::Result<()>,
) -> io::Result<()> {
    if regular(path)?.len() != pkg.bytes {
        return Err(invalid(format!("{} 的文件大小与发布记录不符", pkg.name)));
    }
    check_file(&File::open(path)?, pkg, check)
}

pub(super) fn check_file(
    mut file: &File,
    pkg: Artifact,
    check: impl Fn() -> io::Result<()>,
) -> io::Result<()> {
    if file.metadata()?.len() != pkg.bytes {
        return Err(invalid(format!("{} 的文件大小与发布记录不符", pkg.name)));
    }
    file.rewind()?;
    // 即使另一个进程持续追加，这次校验的读取量也有界。
    let input = file.take(pkg.bytes + 1);
    let actual = match pkg.digest {
        Digest::Sha1(_) => hash::<sha1::Sha1>(input, check)?,
        Digest::Sha256(_) => hash::<sha2::Sha256>(input, check)?,
    };
    if actual != pkg.digest.hex() {
        return Err(invalid(format!("{} 的校验和不匹配", pkg.name)));
    }
    Ok(())
}

pub(super) fn hash<H: Hasher>(
    mut input: impl Read,
    check: impl Fn() -> io::Result<()>,
) -> io::Result<String> {
    let mut h = H::new();
    let mut buffer = vec![0; 256 * 1024];
    loop {
        check()?;
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        h.update(&buffer[..count]);
    }
    Ok(base16ct::lower::encode_string(&h.finalize()))
}
