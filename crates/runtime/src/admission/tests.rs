use super::*;
use std::sync::{
    Barrier,
    atomic::{AtomicUsize, Ordering},
};

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        static SERIAL: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "windroid-admission-test-{}-{}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        Self(dir)
    }
    fn controller(&self, available: u64) -> Admission {
        Admission::new(self.0.clone(), move || {
            MemoryObservation::current(MemoryPressure::Normal, available)
        })
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn guest(cores: u32, ram_mib: u32) -> Demand {
    Demand { cores, ram_mib }
}
fn host(cores: u32, gib: u64) -> Budget {
    Budget {
        logical_cores: cores,
        physical_mib: gib << 10,
    }
}
fn sample(available: u64, time: u64) -> MemoryObservation {
    MemoryObservation {
        pressure: MemoryPressure::Normal,
        additional_mib: available,
        sampled_at: time,
    }
}

#[test]
fn full_cpu_and_memory_budgets_are_inclusive_on_different_hosts() {
    for (cores, gib) in [(4, 8), (10, 16), (20, 64)] {
        let d = Temp::new();
        let admission = d.controller(gib << 10);
        let budget = host(cores, gib);
        let first = admission
            .reserve(5554, guest(cores - 1, (gib << 9) as u32 - 2560), budget)
            .unwrap();
        let exceeds_ram = admission
            .reserve(5556, guest(1, 2561), budget)
            .err()
            .unwrap();
        assert!(exceeds_ram.to_string().contains("共享额度剩余 2560 MiB"));
        let exceeds_cpu = admission
            .reserve(5556, guest(2, 2560), budget)
            .err()
            .unwrap();
        assert!(exceeds_cpu.to_string().contains("当前可分配 1 核"));
        let second = admission.reserve(5556, guest(1, 2560), budget).unwrap();
        drop((first, second));
        // 异常退出或 spawn 失败后关闭最后一个 fd，旧记录立即失效。
        assert!(
            admission
                .reserve(5554, guest(cores, (gib << 9) as u32), budget)
                .is_ok()
        );
    }
}

#[test]
fn a_fourth_guest_fits_when_both_budgets_and_its_port_are_available() {
    let dir = Temp::new();
    let admission = dir.controller(8192);
    let guests: Vec<_> = [5554, 5556, 5558, 5560]
        .into_iter()
        .map(|port| {
            admission
                .reserve(port, guest(2, 2048), host(8, 16))
                .unwrap()
        })
        .collect();
    assert!(
        admission
            .reserve(5560, guest(1, 2048), host(32, 64))
            .is_err()
    );
    drop(guests);
}

#[test]
fn simultaneous_requests_cannot_spend_the_same_headroom_twice() {
    let dir = Temp::new();
    let barrier = Barrier::new(2);
    std::thread::scope(|scope| {
        let workers: Vec<_> = [5554, 5556]
            .into_iter()
            .map(|port| {
                let controller = dir.controller(3000);
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    controller.reserve(port, guest(1, 2560), host(10, 16))
                })
            })
            .collect();
        let results: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
        assert!(
            results
                .into_iter()
                .find_map(Result::err)
                .unwrap()
                .to_string()
                .contains("440 MiB")
        );
    });
    assert!(
        dir.controller(2560)
            .reserve(5554, guest(1, 2560), host(10, 16))
            .is_ok()
    );
}

#[test]
fn only_post_sample_grants_count_toward_headroom_even_if_an_older_guest_exits() {
    let now = 5 * SAMPLE_LIFETIME_NS;
    let old = Grant {
        port: 5554,
        since: 100,
        demand: guest(2, 4096),
        sample: sample(8192, 99),
    };
    let observed = sample(4096, now - 3);
    let newer = Grant {
        port: 5556,
        since: now - 2,
        demand: guest(2, 2560),
        sample: observed,
    };
    let other = guest(2, 1536);
    for current in [&[old, newer][..], &[newer][..]] {
        assert!(check(current, None, other, host(10, 32), observed, now).is_ok());
        assert!(check(current, None, guest(1, 1537), host(10, 32), observed, now).is_err());
    }
    // 已经在采样前运行的 VM 没有被扣两次。重启计算新增量，最终启动仍需全额检查。
    assert!(
        check(
            &[old],
            Some(old),
            old.demand,
            host(10, 16),
            sample(0, now),
            now
        )
        .is_ok()
    );
    assert!(
        check(
            &[old],
            Some(old),
            guest(2, 4097),
            host(10, 16),
            sample(0, now),
            now
        )
        .is_err()
    );
    assert!(check(&[], None, old.demand, host(10, 16), sample(4095, now), now).is_err());
}

#[test]
fn stale_missing_and_critical_observations_are_never_unlimited() {
    let dir = Temp::new();
    for missing in [true, false] {
        let count = Arc::new(AtomicUsize::new(0));
        let calls = count.clone();
        let admission = Admission::new(dir.0.clone(), move || {
            calls.fetch_add(1, Ordering::SeqCst);
            if missing {
                Err(io::Error::other("读取失败"))
            } else {
                Ok(sample(32768, 0))
            }
        });
        assert!(
            admission
                .reserve(5554, guest(1, 2048), host(4, 8))
                .err()
                .unwrap()
                .to_string()
                .contains("重试")
        );
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }
    let count = AtomicUsize::new(0);
    let recovered = Admission::new(dir.0.clone(), move || {
        if count.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(io::Error::other("暂时不可用"));
        }
        MemoryObservation::current(MemoryPressure::Warning, 2048)
    });
    let current = recovered.reserve(5554, guest(1, 2048), host(4, 8)).unwrap();
    let critical = Admission::new(dir.0.clone(), || {
        MemoryObservation::current(MemoryPressure::Critical, 32000)
    });
    assert!(
        critical
            .check_replacement(&current, guest(1, 2048), host(4, 8))
            .unwrap_err()
            .to_string()
            .contains("压力过高")
    );
    assert!(recovered.reserve(5554, guest(1, 2048), host(4, 8)).is_err());
    drop(current);
    recovered.reserve(5554, guest(1, 2048), host(4, 8)).unwrap();
}

#[test]
fn inherited_grant_survives_launcher_exit_and_is_released_by_child_exit() {
    use std::io::{BufRead, BufReader};
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let dir = Temp::new();
    let admission = dir.controller(2560);
    let grant = admission
        .reserve(5554, guest(1, 2560), host(10, 16))
        .unwrap();
    let fd = grant.file.as_raw_fd();
    let mut cmd = Command::new("/bin/sh");
    cmd.args(["-c", "echo ready; read -r quit"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    unsafe {
        cmd.pre_exec(move || {
            if libc::fcntl(fd, libc::F_SETFD, 0) == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        });
    }
    let mut child = cmd.spawn().unwrap();
    assert_eq!(
        BufReader::new(child.stdout.take().unwrap())
            .lines()
            .next()
            .unwrap()
            .unwrap(),
        "ready"
    );
    drop(grant);
    assert!(
        admission
            .reserve(5554, guest(1, 2560), host(10, 16))
            .is_err()
    );
    child.stdin.take().unwrap().write_all(b"exit\n").unwrap();
    assert!(child.wait().unwrap().success());
    assert!(
        admission
            .reserve(5554, guest(1, 2560), host(10, 16))
            .is_ok()
    );
}

#[cfg(target_os = "macos")]
#[test]
fn local_pressure_and_mach_page_statistics_are_readable() {
    let value = observation::read().unwrap();
    assert!(value.is_fresh(observation::now().unwrap()));
    println!(
        "宿主压力 {:?}，当前预计可新增 {} MiB",
        value.pressure, value.additional_mib
    );
}
