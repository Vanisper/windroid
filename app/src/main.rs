//! 将原生应用库、实例协调器与按需创建的显示会话装配在同一进程中。
use std::{
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
};

use host::surface::{CloseIntent, Window};
use nsh_platform_macos::objc2::MainThreadMarker;
use nsh_platform_macos::objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};

mod logging;
mod streaming;
mod windows;

fn main() -> Result<(), String> {
    if let Some(result) = runtime::instances::collector_main() {
        return result.map_err(|e| e.to_string());
    }
    let mtm = MainThreadMarker::new().expect("必须在主线程启动");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    // 格式确认可能是首个原生窗口。先让 AppKit 完成启动，模态按钮与辅助功能才可用。
    app.finishLaunching();
    logging::install();
    let run_id = runtime::app_log::AppLog::allocate_id();
    let mut home = runtime::Layout::default_root().root;
    let mut review = host::surface::review_workspace(mtm, &home);
    let (library_window, library, logs) = loop {
        let error = match review {
            Ok(Some(state)) => match open_library(mtm, &home, &state, &run_id) {
                Ok(opened) => break opened,
                Err(error) => error,
            },
            Ok(None) => return Ok(()),
            Err(error) => error,
        };
        let Some((path, state)) = host::surface::choose_another_workspace(mtm, &home, &error)
        else {
            return Ok(());
        };
        home = path;
        review = Ok(Some(state));
    };
    let result = load_ui(&library_window);
    if result.is_ok() {
        library_window.show(mtm);
        app.run();
    }
    windows::shutdown(mtm);
    library.shutdown();
    logs.finish(
        result
            .as_ref()
            .err()
            .map(String::as_str)
            .unwrap_or("Windroid 正常退出"),
    );
    result
}

fn open_library(
    mtm: MainThreadMarker,
    home: &Path,
    state: &runtime::workspace::Workspace,
    run_id: &str,
) -> Result<(Window, Arc<host::Library>, runtime::app_log::AppLog), String> {
    use runtime::workspace::{self, Workspace};

    if *state != Workspace::Current {
        workspace::initialize(home, state).map_err(|e| e.to_string())?;
    }

    // 被拒绝的目录和取消的预放资源预览必须保持只读。
    let logs = runtime::app_log::AppLog::open(home, run_id);
    let app_on_close = NSApplication::sharedApplication(mtm);
    let library_window = Window::new(
        mtm,
        "Windroid",
        (1060.0, 720.0),
        Box::new(move || {
            app_on_close.stop(None);
            CloseIntent::Close
        }),
    );

    let (commands, rx) = mpsc::channel();
    let (events, windows_rx) = mpsc::channel();
    let fake = std::env::var("WINDROID_FAKE").as_deref() == Ok("1");
    let layout = runtime::Layout {
        root: home.to_owned(),
    };
    let library =
        match host::Library::attach(library_window.channel(), layout, fake, commands, windows_rx) {
            Ok(library) => library,
            Err(error) => {
                logs.finish(&format!("应用库打开失败：{error}"));
                return Err(error);
            }
        };
    windows::install(mtm, library_window.channel(), home.to_owned(), rx, events);
    Ok((library_window, library, logs))
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
