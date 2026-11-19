//! 使用独立子进程注入每个测试的工具路径，并行运行时不会改写测试进程环境。
#![cfg(unix)]

use std::env;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
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
    succeeds(f.child("streams").env("WINDROID_TEST_ADB", adb));
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
    succeeds(f.child("timeout").env("WINDROID_TEST_ADB", adb));
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
    succeeds(f.child("timeout").env("WINDROID_TEST_ADB", adb));
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
    let binary = sdk.join("platform-tools/adb");
    fs::copy(adb, &binary).unwrap();
    succeeds(f.child("devices").env("WINDROID_TEST_ADB", binary));
}

#[test]
fn incompatible_running_service_is_inspected_without_stopping_it() {
    use std::io::{Read, Write};
    let f = Fixture::new();
    let program=f.script("adb",r#"if [ "$1" = version ]; then echo 'Android Debug Bridge version 1.0.41'; else echo "$@" > unexpected; fi"#);
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port().to_string();
    std::thread::scope(|threads| {
        threads.spawn(|| {
            for _ in 0..3 {
                let (mut client, _) = listener.accept().unwrap();
                let mut request = [0; 16];
                client.read_exact(&mut request).unwrap();
                assert_eq!(&request, b"000chost:version");
                client.write_all(b"OKAY00040028").unwrap();
            }
        });
        succeeds(
            f.child("incompatible")
                .env(
                    "WINDROID_TEST_ADB",
                    std::env::var_os("WINDROID_QA_ADB")
                        .map(PathBuf::from)
                        .unwrap_or(program),
                )
                .env("ANDROID_ADB_SERVER_PORT", &port)
                .env("ADB_SERVER_SOCKET", format!("tcp:localhost:{port}")),
        );
    });
    assert!(!f.0.join("unexpected").exists());
}

#[test]
#[ignore = "由每个测试用隔离的环境和临时工具单独调用"]
fn isolated_child() {
    let Ok(case) = env::var("WINDROID_ADB_TEST_CASE") else {
        return;
    };
    let program = PathBuf::from(env::var_os("WINDROID_TEST_ADB").unwrap());
    let device = |serial| adb::Device::injected(&program, serial);
    match case.as_str() {
        "incompatible" => {
            let selected = adb::Device::new(&program, "emulator-5554");
            for failure in [
                adb::ensure_server(&program).unwrap_err(),
                adb::devices(&program).unwrap_err(),
                adb::shell(selected, "true").unwrap_err(),
            ] {
                assert!(failure.to_string().contains("不兼容"), "{failure}");
            }
        }
        "streams" => {
            let out = adb::run(device("test serial"), &["shell", "two words"]).unwrap();
            assert_eq!(out.code, Some(9));
            assert_eq!(out.stdout, "O".repeat(512 * 1024));
            assert_eq!(out.stderr, "E".repeat(512 * 1024));
        }
        "timeout" => {
            let start = Instant::now();
            let error = adb::run_with_timeout(device("test"), &["devices"], Duration::from_secs(1))
                .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::TimedOut);
            assert!(
                start.elapsed() < Duration::from_millis(2500),
                "late timeout: {:?}",
                start.elapsed()
            );
        }
        "devices" => assert_eq!(
            adb::Transport::Injected.devices(&program).unwrap(),
            vec![
                ("emulator-5554".into(), "device".into()),
                ("emulator-5556".into(), "offline".into())
            ]
        ),
        _ => panic!("unknown case {case}"),
    }
}
