//! 将原生应用库、实例协调器与按需创建的显示会话装配在同一进程中。
use std::{path::PathBuf, sync::mpsc};

use host::surface::{CloseIntent, Window};
use nsh_platform_macos::objc2::MainThreadMarker;
use nsh_platform_macos::objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};

mod streaming;
mod windows;

fn main() -> Result<(), String> {
    let mtm = MainThreadMarker::new().expect("必须在主线程启动");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    // 格式确认可能是首个原生窗口。先让 AppKit 完成启动，模态按钮与辅助功能才可用。
    app.finishLaunching();
    let layout = runtime::Layout::default_root();
    if !open_workspace(mtm, &layout.root) {
        return Ok(());
    }
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn"))
        .try_init();
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

fn open_workspace(mtm: MainThreadMarker, home: &std::path::Path) -> bool {
    use runtime::workspace::{self, Workspace};
    let result = workspace::inspect(home).and_then(|state| {
        if let Workspace::Resources(names) = &state {
            let detail = format!(
                "{}\n\n已核验下列预放资源：\n{}\n\n确认后将在此目录建立新的实例库。",
                home.display(),
                names.join("\n")
            );
            if !host::surface::workspace_prompt(mtm, "使用此目录新建工作区？", &detail, true)
            {
                return Ok(false);
            }
        }
        if state != Workspace::Current {
            workspace::initialize(home, &state)?;
        }
        Ok(true)
    });
    result.unwrap_or_else(|e| {
        host::surface::workspace_prompt(
            mtm,
            "无法打开此工作区",
            &format!(
                "{}\n\n{}\n\n请将 WINDROID_HOME 设为新的空目录并重新启动 Windroid。",
                home.display(),
                e
            ),
            false,
        );
        false
    })
}
