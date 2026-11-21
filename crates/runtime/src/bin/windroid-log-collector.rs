//! 独立使用 runtime 的客户端和集成测试可登记这个采集入口。原生客户端直接分派自身。
fn main() -> std::process::ExitCode {
    match runtime::instances::collector_main() {
        Some(Ok(())) => std::process::ExitCode::SUCCESS,
        _ => std::process::ExitCode::FAILURE,
    }
}
