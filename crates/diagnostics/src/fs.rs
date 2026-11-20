//! 从已打开的目录逐级 openat。查验与读写使用同一个文件描述符，
//! 即使另一个进程同时替换路径也不会穿过符号链接。
use rustix::fs::{AtFlags, Mode, OFlags, mkdirat, openat, renameat, unlinkat};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

pub(crate) fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

pub(crate) fn valid_instance(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 80
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}
pub(crate) fn valid_run(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
}

/// 风险边界从用户选定的工作区开始，保留 /var 等系统目录别名。
pub(crate) struct Dir(File);

impl Dir {
    pub(crate) fn home(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
            .open(path)?;
        Self::owned(file)
    }
    fn owned(file: File) -> io::Result<Self> {
        if file.metadata()?.uid() != unsafe { libc::geteuid() } {
            return Err(invalid("目录不属于当前用户"));
        }
        Ok(Self(file))
    }
    pub(crate) fn sub(&self, name: &str, create: bool) -> io::Result<Self> {
        if create {
            match self.mkdir(name) {
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
                r => r?,
            }
        }
        let fd = openat(
            &self.0,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        Self::owned(File::from(fd))
    }
    pub(crate) fn mkdir(&self, name: &str) -> io::Result<()> {
        mkdirat(&self.0, name, Mode::from_raw_mode(0o700))?;
        Ok(())
    }
    pub(crate) fn file(&self, name: &str, flags: OFlags) -> io::Result<File> {
        let fd = openat(
            &self.0,
            name,
            flags | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::from_raw_mode(0o600),
        )?;
        let f = File::from(fd);
        let m = f.metadata()?;
        if !m.is_file() || m.nlink() != 1 || m.uid() != unsafe { libc::geteuid() } {
            return Err(invalid("诊断路径必须是当前用户的独立普通文件"));
        }
        Ok(f)
    }
    pub(crate) fn read(&self, name: &str) -> io::Result<File> {
        self.file(name, OFlags::RDONLY)
    }
    pub(crate) fn create(&self, name: &str) -> io::Result<File> {
        self.file(name, OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL)
    }
    pub(crate) fn remove(&self, name: &str, directory: bool) -> io::Result<()> {
        unlinkat(
            &self.0,
            name,
            if directory {
                AtFlags::REMOVEDIR
            } else {
                AtFlags::empty()
            },
        )?;
        Ok(())
    }
    pub(crate) fn names(&self) -> io::Result<Vec<String>> {
        // 重新打开目录，不共享目录迭代的偏移。
        let fd = openat(
            &self.0,
            ".",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let mut names = Vec::new();
        for entry in rustix::fs::Dir::new(fd)? {
            let entry = entry?;
            if let Ok(name) = entry.file_name().to_str()
                && name != "."
                && name != ".."
            {
                names.push(name.to_owned())
            }
        }
        Ok(names)
    }
    pub(crate) fn publish(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        // 每个清单及清理请求均有单一写者；前次崩溃留下的临时文件可回收。
        let temp = format!(".{name}.tmp");
        if let Err(e) = self.remove(&temp, false)
            && e.kind() != io::ErrorKind::NotFound
        {
            return Err(e);
        }
        let mut f = self.create(&temp)?;
        f.write_all(bytes)?;
        renameat(&self.0, temp.as_str(), &self.0, name)?;
        Ok(())
    }
    pub(crate) fn json<T: serde::de::DeserializeOwned>(&self, name: &str) -> io::Result<T> {
        let f = self.read(name)?;
        if f.metadata()?.len() > 16 << 20 {
            return Err(invalid("索引超出读取上限"));
        }
        Ok(serde_json::from_reader(f.take((16 << 20) + 1))?)
    }
    pub(crate) fn instance(&self, id: &str) -> io::Result<Self> {
        if !valid_instance(id) {
            return Err(invalid("实例标识无效"));
        }
        let dir = self.sub("instances", false)?.sub(id, false)?;
        #[derive(serde::Deserialize)]
        struct Identity {
            id: String,
        }
        if dir.json::<Identity>("instance.json")?.id != id {
            return Err(invalid("实例清单与目录标识不符"));
        }
        Ok(dir)
    }
    pub(crate) fn runs(&self, instance: Option<&str>, create: bool) -> io::Result<Self> {
        match instance {
            Some(id) => self
                .instance(id)?
                .sub("logs", create)?
                .sub("emulator", create),
            None => self.sub("logs", create)?.sub("app", create),
        }
    }
}

pub fn new_run_id() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
