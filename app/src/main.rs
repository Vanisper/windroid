//! 将原生应用库、实例协调器与按需创建的显示会话装配在同一进程中。
use std::{path::PathBuf, sync::mpsc};

use host::surface::{CloseIntent, Window};
use nsh_platform_macos::objc2::MainThreadMarker;
use nsh_platform_macos::objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};

mod streaming;
mod windows;

fn main() -> Result<(), String> {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn"))
        .try_init();
    let mtm = MainThreadMarker::new().expect("必须在主线程启动");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    let app_on_close = app.clone();
    let library_window = Window::new(
        mtm,
        "Windroid",
        (1060.0, 720.0),
        Box::new(move || {
            app_on_close.stop(None);
            CloseIntent::Close
        }),
    );

    let layout = runtime::Layout::default_root();
    let home = layout.root.clone();
    let (commands, rx) = mpsc::channel();
    let (events, windows_rx) = mpsc::channel();
    let fake = std::env::var("WINDROID_FAKE").as_deref() == Ok("1");
    let library =
        host::Library::attach(library_window.channel(), layout, fake, commands, windows_rx)?;
    windows::install(mtm, library_window.channel(), home, rx, events);
    load_ui(&library_window)?;
    library_window.show(mtm);
    app.run();
    windows::shutdown(mtm);
    library.shutdown();
    Ok(())
}

fn load_ui(window: &Window) -> Result<(), String> {
    let explicit = std::env::var_os("WINDROID_UI_DIR").map(PathBuf::from);
    let packaged = std::env::current_exe().ok().and_then(|exe| {
        let contents = exe.parent()?.parent()?;
        (contents.file_name()? == "Contents").then(|| contents.join("Resources/ui"))
    });
    let development = || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".nsh/out");
    let path = explicit
        .or(packaged)
        .unwrap_or_else(development)
        .join("bundle.js");
    window.load_bundle_dir(&path).map_err(|e| {
        format!(
            "载入界面 {} 失败：{e}。开发构建请运行 nsh build app -o app/.nsh/out",
            path.display()
        )
    })
}
