//! 持久化实例。实例目录原子发布，显示名仅用于界面，Android 端使用不可重复的 AVD 名。
//! 分配、选择、启动加锁与删除共享一把目录锁。每个在运行的实例另持有独占文件锁。

mod avd;
mod hardware;
pub(crate) mod process;
mod quick_boot;

use std::collections::BTreeSet;
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Ipv6Addr, TcpListener};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use crate::{Layout, serial_for};
pub(crate) use avd::{data_dir, image_relative};
use contract::{InstanceId, Spec};
pub use hardware::InstanceConfig;
use serde::{Deserialize, Serialize};

/// 各模板分配的客户机内存，用于 AVD 配置与设备域资源准入。
pub fn ram_mib(template: &str) -> u64 {
    match template {
        "compact-720" => 1536,
        "tablet-1200" => 2560,
        "tv-4k" => 3072,
        _ => 2048,
    }
}

/// 读取 macOS 当前机器的物理内存容量。
#[cfg(target_os = "macos")]
pub fn physical_memory_mib() -> io::Result<u64> {
    let mut bytes = 0u64;
    let mut len = std::mem::size_of_val(&bytes);
    // SAFETY: hw.memsize 的值为 uint64_t，目标缓冲区大小匹配。
    let rc = unsafe {
        libc::sysctlbyname(
            c"hw.memsize".as_ptr(),
            (&mut bytes as *mut u64).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(bytes / (1024 * 1024))
}

#[cfg(not(target_os = "macos"))]
pub fn physical_memory_mib() -> io::Result<u64> {
    let pages = unsafe { libc::sysconf(libc::_SC_PHYS_PAGES) };
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if pages < 0 || size < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((pages as u64).saturating_mul(size as u64) / (1024 * 1024))
}

const FIRST_PORT: u16 = 5554;
const LAST_PORT: u16 = 5584;
const MANIFEST: &str = "instance.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceRecord {
    pub id: InstanceId,
    pub name: String,
    pub port: u16,
    pub spec: Spec,
    avd_name: String,
}

impl InstanceRecord {
    pub fn serial(&self) -> String {
        serial_for(self.port)
    }
    pub fn avd_name(&self) -> &str {
        &self.avd_name
    }

    /// Android Studio 使用的只读 AVD 身份属性。serial 只是会被重用的传输地址。
    /// https://android.googlesource.com/platform/tools/base/+/HEAD/adblib/src/com/android/adblib/DevicePropertyNames.kt
    pub fn verify_device(&self) -> io::Result<()> {
        let serial = self.serial();
        let output = adb::shell(&serial, "getprop ro.boot.qemu.avd_name")?;
        if output.code != Some(0) {
            let detail = output.stderr.lines().next().unwrap_or("adb 未返回诊断");
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                format!(
                    "读取 {serial} 的私有 AVD 标识失败（{:?}）：{detail}",
                    output.code
                ),
            ));
        }
        let name = output.stdout.trim();
        if name != self.avd_name {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{serial} 私有 AVD 标识与实例「{}」不符（设备回报 {name:?}）",
                    self.name
                ),
            ));
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct InstanceStore {
    layout: Layout,
}

impl InstanceStore {
    pub fn new(layout: Layout) -> Self {
        Self { layout }
    }

    fn locked(&self) -> io::Result<(PathBuf, File)> {
        private_dir(&self.layout.root)?;
        let root = self.layout.root.canonicalize()?.join("instances");
        private_dir(&root)?;
        let lock = lock_file(&root.join(".registry.lock"))?;
        lock.lock()?;
        Ok((root, lock))
    }

    pub fn list(&self) -> io::Result<Vec<InstanceRecord>> {
        let (root, _lock) = self.locked()?;
        records_in(&root)
    }

    pub fn get(&self, id: &InstanceId) -> io::Result<InstanceRecord> {
        let (root, _lock) = self.locked()?;
        read_record(&root, id)
    }

    pub fn create(&self, name: &str, spec: Spec) -> io::Result<InstanceRecord> {
        self.create_inner(None, name, spec)
    }

    // Backend::Create 的旧调用者提供自己的 id 和端口，走同一套发布与校验流程。
    pub(crate) fn create_at(
        &self,
        id: &InstanceId,
        port: u16,
        spec: Spec,
    ) -> io::Result<InstanceRecord> {
        self.create_inner(Some((id, port)), id.as_str(), spec)
    }

    fn create_inner(
        &self,
        at: Option<(&InstanceId, u16)>,
        name: &str,
        spec: Spec,
    ) -> io::Result<InstanceRecord> {
        let name = valid_name(name)?.trim();
        avd::validate(&spec)?;
        let avd_name = format!("wd-{}", nonce()?);
        let id = at.map_or_else(|| InstanceId::new(&avd_name), |(id, _)| id.clone());
        valid_id(&id)?;
        let (root, _lock) = self.locked()?;
        let final_dir = root.join(id.as_str());
        if final_dir.try_exists()? {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "实例目录已存在",
            ));
        }
        check_space(&root, spec.disk_bytes)?;
        let reserved: BTreeSet<u16> = records_in(&root)?.iter().map(|r| r.port).collect();
        let port = match at {
            Some((_, port)) => {
                if reserved.contains(&port) {
                    return Err(in_use(port));
                }
                check_ports(port)?;
                port
            }
            None => (FIRST_PORT..=LAST_PORT)
                .step_by(2)
                .find(|p| !reserved.contains(p) && check_ports(*p).is_ok())
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::AddrInUse, "实例端口已用尽（5554–5585）")
                })?,
        };
        let record = InstanceRecord {
            id,
            name: name.to_owned(),
            port,
            spec,
            avd_name,
        };
        let stage = root.join(format!(".new-{}", record.avd_name));
        fs::DirBuilder::new().mode(0o700).create(&stage)?;
        let result = (|| {
            avd::generate(&stage, &final_dir, &record)?;
            let mut manifest = File::create_new(stage.join(MANIFEST))?;
            serde_json::to_writer_pretty(&mut manifest, &record)?;
            manifest.sync_all()?;
            fs::rename(&stage, &final_dir)?;
            if let Err(error) = write_selection(&root, Some(&record.id)) {
                // 持有目录锁，该目录仍是本次发布的记录。
                fs::remove_dir_all(&final_dir)?;
                return Err(error);
            }
            Ok(record)
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&stage);
        }
        result
    }

    /// 最近一次正常退出写入的 Quick Boot 快照仍可用。宿主在 Stop 完成后调用。
    pub fn quick_boot_saved(&self, id: &InstanceId) -> io::Result<bool> {
        let (root, _lock) = self.locked()?;
        let record = read_record(&root, id)?;
        quick_boot::saved(&root.join(id.as_str()), &record)
    }

    pub fn selected(&self) -> io::Result<Option<InstanceId>> {
        let (root, _lock) = self.locked()?;
        let records = records_in(&root)?;
        selected_in(&root, &records)
    }

    pub fn select(&self, id: &InstanceId) -> io::Result<()> {
        let (root, _lock) = self.locked()?;
        read_record(&root, id)?;
        write_selection(&root, Some(id))
    }

    /// 删除前要求有匹配的持久记录、独占运行锁以及已释放的端口。重试已完成的删除可成功。
    pub fn delete(&self, id: &InstanceId) -> io::Result<()> {
        valid_id(id)?;
        let (root, _lock) = self.locked()?;
        let dir = root.join(id.as_str());
        if let Err(error) = fs::symlink_metadata(&dir) {
            return if error.kind() == io::ErrorKind::NotFound {
                Ok(())
            } else {
                Err(error)
            };
        }
        let record = read_record(&root, id)?;
        let _lease = acquire_run_lock(&dir)?;
        process::ensure_previous_exit(&dir)?;
        check_ports(record.port)?;
        let records = records_in(&root)?;
        if selected_in(&root, &records)?.as_ref() == Some(id) {
            write_selection(&root, records.iter().find(|r| r.id != *id).map(|r| &r.id))?;
        }
        // 重命名从列表中立即移除；只清理刚刚确认归属的目录，目录内的链接不会被追随。
        let retired = root.join(format!(".deleted-{}", record.avd_name));
        fs::rename(dir, &retired)?;
        fs::remove_dir_all(retired)
    }

    /// 启动者带走运行锁后才放开目录锁，避免与另一个进程的删除/启动交错。
    pub(crate) fn lease(
        &self,
        id: &InstanceId,
        port: u16,
    ) -> io::Result<(InstanceRecord, PathBuf, File)> {
        let (root, _lock) = self.locked()?;
        let record = read_record(&root, id)?;
        if record.port != port {
            return Err(invalid("实例记录与请求端口不一致"));
        }
        let dir = root.join(id.as_str());
        let lease = acquire_run_lock(&dir)?;
        process::ensure_previous_exit(&dir)?;
        check_ports(port)?;
        Ok((record, dir, lease))
    }
}

fn acquire_run_lock(dir: &Path) -> io::Result<File> {
    let lease = lock_file(&dir.join("run.lock"))?;
    lease.try_lock().map_err(io::Error::from)?;
    Ok(lease)
}

fn lock_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

fn records_in(root: &Path) -> io::Result<Vec<InstanceRecord>> {
    let mut records = Vec::new();
    let mut used = BTreeSet::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() || entry.file_name().as_encoded_bytes().starts_with(b".") {
            continue;
        }
        // 早期版本的空目录没有发行记录，保留用户数据，同时把它排除在可操作列表外。
        if !entry.path().join(MANIFEST).try_exists()? {
            continue;
        }
        let id = InstanceId::new(
            entry
                .file_name()
                .into_string()
                .map_err(|_| invalid("实例目录名称无效"))?,
        );
        let record = read_record(root, &id)?;
        if !used.insert(record.port) {
            return Err(invalid("已有实例占用相同的端口对"));
        }
        records.push(record);
    }
    records.sort_by_key(|record| record.port);
    Ok(records)
}

fn read_record(root: &Path, id: &InstanceId) -> io::Result<InstanceRecord> {
    valid_id(id)?;
    let dir = root.join(id.as_str());
    if !fs::symlink_metadata(&dir)?.is_dir() {
        return Err(invalid("实例目录类型无效"));
    }
    let manifest = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(dir.join(MANIFEST))?;
    if !manifest.metadata()?.is_file() || manifest.metadata()?.len() > 8192 {
        return Err(invalid("实例记录类型或长度无效"));
    }
    let record: InstanceRecord = serde_json::from_reader(manifest)?;
    let token = record.avd_name.strip_prefix("wd-").unwrap_or("");
    if record.id != *id
        || !valid_port(record.port)
        || token.len() != 32
        || !token.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(invalid("实例归属记录无效"));
    }
    valid_name(&record.name)?;
    avd::validate(&record.spec)?;
    Ok(record)
}

fn selected_in(root: &Path, records: &[InstanceRecord]) -> io::Result<Option<InstanceId>> {
    match fs::read_to_string(root.join(".selected")) {
        Ok(id) => {
            if let Some(record) = records.iter().find(|r| r.id.as_str() == id) {
                return Ok(Some(record.id.clone()));
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => (),
        Err(e) => return Err(e),
    }
    Ok(records.first().map(|r| r.id.clone()))
}

fn write_selection(root: &Path, id: Option<&InstanceId>) -> io::Result<()> {
    let dest = root.join(".selected");
    if let Some(id) = id {
        let temp = root.join(format!(".selection-{}", nonce()?));
        let mut file = File::create_new(&temp)?;
        let result = (|| {
            file.write_all(id.as_str().as_bytes())?;
            file.sync_all()?;
            fs::rename(&temp, dest)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    } else {
        match fs::remove_file(dest) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            result => result,
        }
    }
}

pub(crate) fn valid_id(id: &InstanceId) -> io::Result<()> {
    let id = id.as_str();
    if id.is_empty()
        || id.len() > 80
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return Err(invalid("实例标识无效"));
    }
    Ok(())
}
fn valid_name(name: &str) -> io::Result<&str> {
    if name.trim().is_empty() || name.chars().count() > 64 || name.chars().any(char::is_control) {
        return Err(invalid("请使用 1 至 64 个非控制字符作为实例名称"));
    }
    Ok(name)
}
fn valid_port(port: u16) -> bool {
    (FIRST_PORT..=LAST_PORT).contains(&port) && port.is_multiple_of(2)
}
fn in_use(port: u16) -> io::Error {
    io::Error::new(
        io::ErrorKind::AddrInUse,
        format!("模拟器端口对 {port}/{} 已被占用", port + 1),
    )
}
/// https://developer.android.com/studio/run/emulator-commandline#common
pub(crate) fn check_ports(port: u16) -> io::Result<()> {
    if !valid_port(port) {
        return Err(invalid("模拟器 console 端口必须是 5554 至 5584 之间的偶数"));
    }
    let _v4 = [port, port + 1].map(|p| TcpListener::bind((Ipv4Addr::LOCALHOST, p)));
    let _v6 = [port, port + 1].map(|p| TcpListener::bind((Ipv6Addr::LOCALHOST, p)));
    for result in _v4.into_iter().chain(_v6.into_iter().filter(|r| !matches!(r, Err(e) if e.kind() == io::ErrorKind::AddrNotAvailable || e.kind() == io::ErrorKind::Unsupported))) {
        match result {
            Err(e) if e.kind() == io::ErrorKind::AddrInUse => return Err(in_use(port)),
            Err(e) => return Err(e),
            Ok(_) => (),
        }
    }
    Ok(())
}

fn check_space(root: &Path, bytes: u64) -> io::Result<()> {
    let path = CString::new(root.as_os_str().as_bytes())?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: path 为 NUL 结尾的字节串，成功时 statvfs 填充整个结构。
    let stat = unsafe {
        if libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) != 0 {
            return Err(io::Error::last_os_error());
        }
        stat.assume_init()
    };
    let available = stat.f_bavail as u128 * stat.f_frsize as u128;
    if (bytes as u128) * 10 > available * 7 {
        return Err(io::Error::new(
            io::ErrorKind::StorageFull,
            "数据盘超过当前可用空间的 70%",
        ));
    }
    Ok(())
}

fn nonce() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn private_dir(dir: &Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    if !fs::symlink_metadata(dir)?.is_dir() {
        return Err(invalid("私有实例路径必须是目录"));
    }
    Ok(())
}

pub(crate) fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg.into())
}
