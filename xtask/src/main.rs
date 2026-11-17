//! 构建期工具。
//!
//! 目前只做一件事：把 app/App.tsx 编译成 nsh 能载入的 bundle.js。
//!
//! 为什么不直接用 nsh CLI：Windroid 不用 nsh 自己的 runner（窗口归我们所有），
//! 而 CLI 的构建链与 runner 绑在一起。这里只借它的 bundler，产出物交给自己的
//! 载入路径。

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use nsh_bundler::{BundleOptions, bundle};

fn main() -> ExitCode {
    let root = workspace_root();
    let app = root.join("app");
    let entry = app.join("App.tsx");

    if !entry.is_file() {
        eprintln!("找不到界面入口: {}", entry.display());
        return ExitCode::FAILURE;
    }

    match build(&entry, &app) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

fn build(entry: &Path, out: &Path) -> Result<(), String> {
    let sets: Vec<nsh_core::element::description::SetDescription> = nsh_base::registry()
        .sets()
        .into_iter()
        .map(nsh_core::element::description::SetDescription::from)
        .collect();

    let bundled = bundle(&BundleOptions {
        entry: entry.to_path_buf(),
        minify: false,
        development: true,
        split: false,
        ui_sets: sets,
        ..Default::default()
    })
    .map_err(|e| e.plain)?;

    let js = out.join("bundle.js");
    std::fs::write(&js, &bundled.code).map_err(|e| format!("{}: {e}", js.display()))?;

    if let Some(map) = &bundled.source_map {
        let p = out.join("bundle.js.map");
        std::fs::write(&p, map).map_err(|e| format!("{}: {e}", p.display()))?;
    }

    // 能力声明单独落一份：Bundle::read 会读同目录的 capabilities.json。
    // 这份文件不声明，界面里每个 host.call 都会被拒。
    if let Some(caps) = &bundled.capabilities {
        let p = out.join("capabilities.json");
        std::fs::write(&p, caps.to_json().to_string())
            .map_err(|e| format!("{}: {e}", p.display()))?;
    }

    println!("已生成 {}", js.display());
    Ok(())
}

fn workspace_root() -> PathBuf {
    // xtask 位于工作区根下的 xtask/，因此父目录即根。
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .to_path_buf()
}
