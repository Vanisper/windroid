//! 持久化实例。实例目录原子发布，显示名仅用于界面，Android 端使用不可重复的 AVD 名。
//! 分配、选择、启动加锁与删除共享一把目录锁。每个在运行的实例另持有独占文件锁。

mod avd;
mod hardware;
mod ports;
mod preview;
pub(crate) mod process;
mod quick_boot;
mod specification;

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Ipv6Addr, TcpListener};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use crate::{Layout, serial_for};
pub(crate) use avd::{data_dir, image_subdir};
use contract::{InstanceId, Spec};
pub use hardware::InstanceConfig;
use serde::{Deserialize, Serialize};
pub use specification::{DiskLimits, GraphicsMode, Hardware, HostCapacity, SpecLimits};

/// 新格式首版模板的保存值。创建时提升至当前镜像下限，已有 AVD 保留原值。
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
    pub fn verify_device(&self, program: &Path) -> io::Result<()> {
        let serial = self.serial();
        let output = adb::shell(
            adb::Device::new(program, &serial),
            "getprop ro.boot.qemu.avd_name",
        )?;
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

    pub(crate) fn home(&self) -> &Path {
        &self.layout.root
    }

    fn locked(&self) -> io::Result<(PathBuf, File)> {
        private_dir(&self.layout.root)?;
        let root = self.layout.root.canonicalize()?.join("instances");
        private_dir(&root)?;
        let lock = lock_file(&root.join(".registry.lock"))?;
        lock.lock()?;
        Ok((root, lock))
    }

    pub(crate) fn list_readonly(&self) -> io::Result<Vec<InstanceRecord>> {
        let root = self.layout.root.join("instances");
        match fs::symlink_metadata(&root) {
            Ok(metadata) if metadata.is_dir() => records_in(&root),
            Ok(_) => Err(invalid("实例注册目录类型异常")),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e),
        }
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
        self.create_inner(None, name, Hardware::original(&spec.template), spec)
    }

    /// 在根设置读锁内核对将要持久化的修订。跨工作区切换或准备与创建相交时，
    /// 界面此前显示的就绪快照不能替代提交前的本地核验。
    pub fn create_prepared(&self, name: &str, spec: Spec) -> io::Result<InstanceRecord> {
        self.create_checked(name, spec, None)
    }

    pub fn create_with_hardware(
        &self,
        name: &str,
        spec: Spec,
        hardware: Hardware,
    ) -> io::Result<InstanceRecord> {
        self.create_checked(name, spec, Some(hardware))
    }

    fn create_checked(
        &self,
        name: &str,
        spec: Spec,
        hardware: Option<Hardware>,
    ) -> io::Result<InstanceRecord> {
        avd::validate(&spec)?;
        let _selection = crate::storage::resource_lease(self.home(), true, true)?;
        let (paths, _versions) = crate::storage::instance_lease(self.home(), &spec.image)?;
        let name_in_image = specification::product_name(&paths, &spec.image)?;
        let limits = SpecLimits::for_template(
            HostCapacity::read()?,
            &spec.template,
            spec.image.api,
            &name_in_image,
        )?;
        let hardware = hardware.unwrap_or_else(|| limits.defaults());
        limits.validate(hardware)?;
        self.create_inner(None, name, hardware, spec)
    }

    // Backend::Create 的旧调用者提供自己的 id 和端口，走同一套发布与校验流程。
    pub(crate) fn create_at(
        &self,
        id: &InstanceId,
        port: u16,
        spec: Spec,
    ) -> io::Result<InstanceRecord> {
        self.create_inner(
            Some((id, port)),
            id.as_str(),
            Hardware::original(&spec.template),
            spec,
        )
    }

    fn create_inner(
        &self,
        at: Option<(&InstanceId, u16)>,
        name: &str,
        hardware: Hardware,
        spec: Spec,
    ) -> io::Result<InstanceRecord> {
        let name = valid_name(name)?.trim();
        avd::validate(&spec)?;
        let avd_name = format!("wd-{}", nonce()?);
        let id = at.map_or_else(|| InstanceId::new(&avd_name), |(id, _)| id.clone());
        valid_id(&id)?;
        let _resources = crate::storage::resource_lease(self.home(), true, true)?;
        let (root, _lock) = self.locked()?;
        let final_dir = root.join(id.as_str());
        if final_dir.try_exists()? {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "实例目录已存在",
            ));
        }
        self.disk_limits()?.check(spec.disk_bytes)?;
        let reserved: BTreeSet<u16> = records_in(&root)?.iter().map(|r| r.port).collect();
        let occupied = ports::Occupied::read()?;
        let port = match at {
            Some((_, port)) => {
                if reserved.contains(&port) {
                    return Err(in_use(port));
                }
                check_ports(port)?;
                occupied.check(port)?;
                port
            }
            None => (FIRST_PORT..=LAST_PORT)
                .step_by(2)
                .find(|p| {
                    !reserved.contains(p) && occupied.check(*p).is_ok() && check_ports(*p).is_ok()
                })
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
            let paths = crate::provision::RuntimePaths::discover(&self.layout.root);
            avd::generate(&stage, &final_dir, &record, &paths, hardware)?;
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
        ports::Occupied::read()?.check(port)?;
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

pub(crate) fn inspect_registry(root: &Path) -> io::Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        if name.as_encoded_bytes().starts_with(b".") {
            let kind = entry.file_type()?;
            let name = name.to_str().ok_or_else(|| invalid("注册表文件名无效"))?;
            let known = match name {
                ".registry.lock" => {
                    crate::workspace::access(&entry.path(), libc::R_OK | libc::W_OK)?;
                    use std::os::unix::fs::MetadataExt;
                    let meta = entry.metadata()?;
                    kind.is_file() && meta.len() == 0 && meta.nlink() == 1
                }
                ".selected" => kind.is_file(),
                _ if name.starts_with(".new-") || name.starts_with(".deleted-") => kind.is_dir(),
                _ if name.starts_with(".selection-") => kind.is_file(),
                _ => false,
            };
            if !known {
                return Err(invalid(format!("注册表内存在未知文件或类型：{name}")));
            }
        } else if !entry.file_type()?.is_dir() || !entry.path().join(MANIFEST).try_exists()? {
            return Err(invalid(format!(
                "实例目录缺少有效记录：{}",
                entry.path().display()
            )));
        }
    }
    let records = records_in(root)?;
    selected_in(root, &records).map(|_| ()).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("{}：{e}", root.join(".selected").display()),
        )
    })
}

fn records_in(root: &Path) -> io::Result<Vec<InstanceRecord>> {
    let mut records = Vec::new();
    let mut used = BTreeSet::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() || entry.file_name().as_encoded_bytes().starts_with(b".") {
            continue;
        }
        let id = InstanceId::new(
            entry
                .file_name()
                .into_string()
                .map_err(|_| invalid("实例目录名称无效"))?,
        );
        let record = read_record(root, &id).map_err(|e| {
            let path = entry.path().join(MANIFEST);
            io::Error::new(e.kind(), format!("{}：{e}", path.display()))
        })?;
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
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
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
    match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(root.join(".selected"))
    {
        Ok(file) => {
            if !file.metadata()?.is_file() {
                return Err(invalid("实例选择记录类型无效"));
            }
            let mut text = String::new();
            file.take(81).read_to_string(&mut text)?;
            let id = InstanceId::new(text);
            valid_id(&id)?;
            if let Some(record) = records.iter().find(|r| r.id == id) {
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

pub(crate) fn nonce() -> io::Result<String> {
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
