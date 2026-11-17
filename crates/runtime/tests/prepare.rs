//! 自检的测试：只验证判断逻辑，不依赖本机装了什么。

use runtime::prepare::{check, Component};

#[test]
fn readiness_reports_each_component() {
    let r = check();
    // 本机未必装齐，但三项必须都被检查到——漏检比检出缺失更糟。
    let names: Vec<&str> = r.components.iter().map(|c| c.name).collect();
    assert_eq!(names, ["Android 模拟器", "adb", "scrcpy-server"]);
}

#[test]
fn missing_components_carry_remedy() {
    let r = check();
    for c in r.missing() {
        assert!(
            !c.remedy.is_empty(),
            "缺失项 {} 必须说明后果与补法",
            c.name
        );
        assert!(c.path.is_none(), "不可用的项不该报告路径");
    }
}

#[test]
fn usable_requires_components_and_avd() {
    let r = check();
    // 两者缺一不可：没有 AVD 无法创建实例，缺组件无法启动。
    assert_eq!(r.usable(), r.components.iter().all(|c: &Component| c.ok) && !r.avds.is_empty());
}
