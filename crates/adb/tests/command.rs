//! 使用独立子进程注入每个测试的工具路径，并行运行时不会改写测试进程环境。
#![cfg(unix)]

use std::env;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = env::temp_dir().join(format!(
            "windroid-adb-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn script(&self, name: &str, body: &str) -> PathBuf {
        let path = self.0.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    fn child(&self, case: &str) -> Command {
        let mut cmd = Command::new(env::current_exe().unwrap());
        cmd.args(["--ignored", "--exact", "isolated_child"])
            .current_dir(&self.0)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &self.0)
            .env("WINDROID_ADB_TEST_CASE", case);
        cmd
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn succeeds(cmd: &mut Command) {
    // 外层单独监视测试 helper，回归造成的死锁也能报告成失败。
    let mut child = cmd.stdin(std::process::Stdio::null()).spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            child.wait().unwrap();
            panic!("isolated helper timed out");
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    assert!(status.success(), "isolated helper exited with {status}");
}

#[test]
fn runtime_and_explicit_paths_take_precedence() {
    let f = Fixture::new();
    let home = f.0.join("windroid home");
    let sdk = f.0.join("development sdk");
    let managed = home.join("runtime/sdk/platform-tools/adb");
    let explicit = f.0.join("adb override");
    let path = |path: &Path| {
        let mut cmd = f.child("path");
        cmd.env("WINDROID_EXPECTED_PATH", path);
        cmd
    };
    succeeds(&mut path(
        &f.0.join(".windroid/runtime/sdk/platform-tools/adb"),
    ));
    succeeds(
        path(&f.0.join(".windroid/runtime/sdk/platform-tools/adb"))
            .env("WINDROID_ADB", "")
            .env("WINDROID_SDK", "")
            .env("WINDROID_HOME", ""),
    );
    succeeds(path(&managed).env("WINDROID_HOME", &home));
    succeeds(
        path(&sdk.join("platform-tools/adb"))
            .env("WINDROID_SDK", &sdk)
            .env("WINDROID_HOME", &home),
    );
    succeeds(
        path(&explicit)
            .env("WINDROID_ADB", &explicit)
            .env("WINDROID_SDK", &sdk),
    );

    // 单独指定 serial 的旧手工入口支持 PATH；显式指定运行时目录始终使用该目录。
    succeeds(path(Path::new("adb")).env("WINDROID_SERIAL", "emulator-5554"));
    succeeds(
        path(&managed)
            .env("WINDROID_SERIAL", "emulator-5554")
            .env("WINDROID_HOME", &home),
    );
    let default_adb = f.0.join(".windroid/runtime/sdk/platform-tools/adb");
    fs::create_dir_all(default_adb.parent().unwrap()).unwrap();
    fs::write(&default_adb, []).unwrap();
    succeeds(path(&default_adb).env("WINDROID_SERIAL", "emulator-5554"));
    let settings = windroid_locations::StorageLocations {
        runtime_root: f.0.join("moved tools"),
        image_root: Some(f.0.join("independent images")),
    };
    fs::create_dir_all(&home).unwrap();
    fs::write(
        home.join(windroid_locations::SETTINGS),
        format!(
            r#"{{"runtimeRoot":"{}","imageRoot":null}}"#,
            settings.runtime_root.display()
        ),
    )
    .unwrap();
    succeeds(
        path(&settings.runtime_root.join("sdk/platform-tools/adb")).env("WINDROID_HOME", &home),
    );
    succeeds(
        path(&sdk.join("platform-tools/adb"))
            .env("WINDROID_HOME", &home)
            .env("WINDROID_SDK", &sdk),
    );
}

#[test]
fn both_streams_larger_than_the_pipe_capacity_are_captured() {
    let f = Fixture::new();
    let adb = f.script(
        "fake-adb",
        r#"
[ "$#" = 4 ] && [ "$1" = '-s' ] && [ "$2" = 'test serial' ] &&
[ "$3" = 'shell' ] && [ "$4" = 'two words' ] || exit 90
dd if=/dev/zero bs=65536 count=8 2>/dev/null | tr '\000' 'E' >&2
dd if=/dev/zero bs=65536 count=8 2>/dev/null | tr '\000' 'O'
exit 9
"#,
    );
    succeeds(f.child("streams").env("WINDROID_ADB", adb));
}

#[test]
fn excessive_output_fails_for_either_stream() {
    for (name, redirect) in [("stdout", ""), ("stderr", ">&2")] {
        // 先复制目标写端，再用 /dev/null 丢弃 dd 自身的统计信息。
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg(format!(
            "exec dd if=/dev/zero bs=65536 count=129 {redirect} 2>/dev/null"
        ));
        match adb::output_with_timeout(cmd, Duration::from_secs(10)) {
            Err(e) => {
                assert_eq!(e.kind(), io::ErrorKind::InvalidData);
                assert!(e.to_string().contains(name), "{e}");
            }
            Ok(_) => panic!("oversized {name} accepted"),
        }
    }
}

#[test]
fn expired_command_is_killed_and_reaped() {
    let f = Fixture::new();
    let adb = f.script("slow-adb", "echo $$ > adb.pid\nexec sleep 10");
    succeeds(f.child("timeout").env("WINDROID_ADB", adb));
    let pid = fs::read_to_string(f.0.join("adb.pid")).unwrap();
    // kill -0 对运行中及未回收的僵尸都会成功。
    assert!(
        !Command::new("/bin/kill")
            .args(["-0", pid.trim()])
            .output()
            .unwrap()
            .status
            .success()
    );
}

#[test]
fn deadline_covers_writers_inherited_by_descendants() {
    let f = Fixture::new();
    // 即使直接子进程先退出，stdout 和 stderr 仍可被后代打开。
    let adb = f.script("leaked-writer", "sleep 3 &\nexit 0");
    succeeds(f.child("timeout").env("WINDROID_ADB", adb));
}

#[test]
fn device_list_uses_the_managed_binary_and_parses_its_output() {
    let f = Fixture::new();
    let sdk = f.0.join("managed SDK");
    fs::create_dir_all(sdk.join("platform-tools")).unwrap();
    let adb = f.script(
        "list",
        r#"
[ "$#" = 1 ] && [ "$1" = 'devices' ] || exit 90
printf 'List of devices attached\nemulator-5554\tdevice\nemulator-5556\toffline\n\n'
"#,
    );
    fs::copy(adb, sdk.join("platform-tools/adb")).unwrap();
    succeeds(f.child("devices").env("WINDROID_SDK", sdk));
}

#[test]
#[ignore = "由每个测试用隔离的环境和临时工具单独调用"]
fn isolated_child() {
    let Ok(case) = env::var("WINDROID_ADB_TEST_CASE") else {
        return;
    };
    match case.as_str() {
        "path" => assert_eq!(
            adb::program().unwrap(),
            PathBuf::from(env::var_os("WINDROID_EXPECTED_PATH").unwrap())
        ),
        "streams" => {
            let out = adb::run("test serial", &["shell", "two words"]).unwrap();
            assert_eq!(out.code, Some(9));
            assert_eq!(out.stdout, "O".repeat(512 * 1024));
            assert_eq!(out.stderr, "E".repeat(512 * 1024));
        }
        "timeout" => {
            let start = Instant::now();
            let error =
                adb::run_with_timeout("test", &["devices"], Duration::from_secs(1)).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::TimedOut);
            assert!(
                start.elapsed() < Duration::from_millis(2500),
                "late timeout: {:?}",
                start.elapsed()
            );
        }
        "devices" => assert_eq!(
            adb::devices().unwrap(),
            vec![
                ("emulator-5554".into(), "device".into()),
                ("emulator-5556".into(), "offline".into())
            ]
        ),
        _ => panic!("unknown case {case}"),
    }
}
