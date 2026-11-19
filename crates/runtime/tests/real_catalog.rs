//! 真实设备的应用清单与桌面状态。
//!
//! 需要一台已就绪的模拟器，默认忽略：
//!
//!   WINDROID_TEST_ADB=/absolute/path/to/adb WINDROID_SERIAL=emulator-5554 cargo test -p runtime --test real_catalog -- --ignored --nocapture

use runtime::catalog;

fn serial() -> String {
    std::env::var("WINDROID_SERIAL").expect("请指定本次验收实例的 WINDROID_SERIAL")
}

#[test]
#[ignore = "需要一台已就绪的模拟器"]
fn real_device_catalog_lists_launchable_apps() {
    let program =
        std::path::PathBuf::from(std::env::var_os("WINDROID_TEST_ADB").expect("指定测试用 adb"));
    let apps = catalog::snapshot(adb::Device::new(&program, &serial())).expect("读取应用清单");
    println!("共 {} 个应用", apps.len());
    for a in apps.iter().take(6) {
        println!(
            "  {} system={} launcher={}",
            a.package, a.system, a.launcher
        );
    }

    assert!(!apps.is_empty(), "真实设备应有可启动的应用");
    // 进清单的硬条件是能启动：不能启动的包在应用库里无事可做。
    for a in &apps {
        assert!(a.launcher, "{} 不能启动，不该出现在清单里", a.package);
    }
    // 系统与用户两类都要有，否则分类逻辑没被真正走过。
    assert!(apps.iter().any(|a| a.system), "应包含系统应用");
    assert!(apps.iter().any(|a| !a.system), "应包含用户安装的应用");
}

#[test]
#[ignore = "需要一台已就绪的模拟器"]
fn real_device_reports_desktop_state() {
    let program =
        std::path::PathBuf::from(std::env::var_os("WINDROID_TEST_ADB").expect("指定测试用 adb"));
    let home = catalog::home_state(adb::Device::new(&program, &serial())).expect("读取桌面状态");
    println!("候选 {:?}", home.candidates);
    println!("默认 {:?}", home.default);
    assert!(!home.candidates.is_empty(), "真实设备应有桌面候选");
    // 默认桌面必须在候选里：否则「设为默认桌面」的操作无从派生。
    if let Some(default) = &home.default {
        assert!(home.is_candidate(default), "默认桌面 {default} 不在候选中");
    }
}
