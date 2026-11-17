//! 构建期工具。
//!
//! 两件事：把 `app/App.tsx` 编译成 nsh 能载入的 bundle，以及生成编辑器的
//! TypeScript 类型声明。
//!
//! 为什么不直接用 nsh CLI：Windroid 不用 nsh 自己的 runner（窗口归我们所有），
//! 而 CLI 的构建链与 runner 绑在一起。这里借它的 bundler 与声明生成器，
//! 产出物交给自己的载入路径。
//!
//! 类型声明不是可选项。没有 `.nsh/types` 时编辑器里每个 `from "nsh"` 都是红的，
//! 而这与运行期无关——bundler 不看类型。两者必须一起跑，否则「能跑但满屏报错」。

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use nsh_bundler::{BundleOptions, bundle};
use nsh_core::element::description::SetDescription;

fn main() -> ExitCode {
    let root = workspace_root();
    let app = root.join("app");

    match run(&app) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

fn run(app: &Path) -> Result<(), String> {
    let entry = app.join("App.tsx");
    if !entry.is_file() {
        return Err(format!("找不到界面入口: {}", entry.display()));
    }
    write_types(app)?;
    build(&entry, app)?;
    check_types(app)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 类型检查
// ---------------------------------------------------------------------------

/// 用 tsc 检查界面源码。
///
/// 这一步不是可选的严格性：bundler 只看语法，不做类型检查。界面里一个拼错的
/// 标识符或一个改了名却漏改的字段，构建完全正常，运行到那行才炸——而界面跑在
/// QuickJS 里，崩溃表现为「点了没反应」，最难定位。
///
/// 本机没有 tsc 时只告警不失败：编译产物本身是有效的，不该因为缺少可选工具
/// 而阻断构建。但告警必须说清后果，否则等于静默降级。
fn check_types(app: &Path) -> Result<(), String> {
    let Some(tsc) = find_tsc(app) else {
        println!(
            "提示：未找到 tsc，跳过类型检查。\n\
             界面在运行期才暴露类型错误。安装方式：npm i -D typescript（或全局安装）。"
        );
        return Ok(());
    };

    let out = std::process::Command::new(&tsc)
        .args(["-p", "tsconfig.json", "--noEmit"])
        .current_dir(app)
        .output()
        .map_err(|e| format!("运行 {} 失败: {e}", tsc.display()))?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stdout);
        return Err(format!("类型检查未通过:\n{}", stderr.trim_end()));
    }
    println!("类型检查通过");
    Ok(())
}

/// 找 tsc：优先应用内的 node_modules，其次 PATH。
///
/// 仓库不强制 node_modules——nsh 的类型来自 `.nsh/types`，不需要它。
/// 但 tsc 这个可执行文件需要，所以两种来源都认。
fn find_tsc(app: &Path) -> Option<PathBuf> {
    let local = app.join("node_modules/.bin/tsc");
    if local.is_file() {
        return Some(local);
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join("tsc"))
            .find(|p| p.is_file())
    })
}

/// UI 描述集合。bundler 与声明生成器必须用同一份，否则声明的类型与实际能用的
/// 组件会对不上。
fn sets() -> Vec<SetDescription> {
    nsh_base::registry()
        .sets()
        .into_iter()
        .map(SetDescription::from)
        .collect()
}

// ---------------------------------------------------------------------------
// TypeScript 类型声明
// ---------------------------------------------------------------------------

/// 生成 `.nsh/types` 与 `.nsh/tsconfig.json`。
///
/// 与 `nsh init` 写的是同一批文件、同一份内容——这不是自创格式，是照官方
/// 布局复刻。应用的 `tsconfig.json` 通过 `extends` 指过来，于是 `nsh` 的
/// 每个导入都能解析到声明，编辑器不再把 `from "nsh"` 标红。
///
/// 内容未变的文件跳过重写：编辑器看到文件时间戳变化会重新加载，无谓地打断输入。
fn write_types(dir: &Path) -> Result<(), String> {
    let types = dir.join(".nsh/types");
    std::fs::create_dir_all(&types).map_err(|e| format!("{}: {e}", types.display()))?;

    let mut definitions = nsh_js::type_definitions(&sets());
    definitions.extend(nsh_bundler::type_definitions());

    // 主题令牌取自 nsh.json 的 "theme"；读不出来时退回默认主题，
    // 与 nsh init 的处理一致——类型不该因为主题写错而缺失。
    let tokens = nsh_bundler::theme_tokens(dir)
        .unwrap_or_else(|_| nsh_base::Theme::default().into_tokens());
    definitions.push(("nsh/tokens.d.ts".to_owned(), nsh_bundler::tokens_declaration(&tokens)));

    for (rel, contents) in &definitions {
        let path = types.join(rel);
        if std::fs::read(&path).is_ok_and(|old| old == contents.as_bytes()) {
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        std::fs::write(&path, contents).map_err(|e| format!("{}: {e}", path.display()))?;
    }

    let tsconfig = dir.join(".nsh/tsconfig.json");
    if !std::fs::read(&tsconfig).is_ok_and(|old| old == nsh_bundler::TSCONFIG_BASE.as_bytes()) {
        std::fs::write(&tsconfig, nsh_bundler::TSCONFIG_BASE)
            .map_err(|e| format!("{}: {e}", tsconfig.display()))?;
    }

    // 应用自己的 tsconfig.json：只做 extend，不复制任何编译选项。
    // nsh 升级时改 .nsh/tsconfig.json 即可，应用侧无需跟着改。
    let app_tsconfig = dir.join("tsconfig.json");
    if !app_tsconfig.is_file() {
        std::fs::write(&app_tsconfig, nsh_bundler::TSCONFIG)
            .map_err(|e| format!("{}: {e}", app_tsconfig.display()))?;
    }

    println!("已生成 {}", types.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// 界面编译
// ---------------------------------------------------------------------------

fn build(entry: &Path, out: &Path) -> Result<(), String> {
    let bundled = bundle(&BundleOptions {
        entry: entry.to_path_buf(),
        minify: false,
        development: true,
        split: false,
        ui_sets: sets(),
        ..Default::default()
    })
    .map_err(|e| e.plain)?;

    // 产物落在 .nsh/out，与源码分开：bundle.js 是构建输出，不该进版本库，
    // 也不该混在 App.tsx 旁边。
    let out_dir = out.join(".nsh/out");
    std::fs::create_dir_all(&out_dir).map_err(|e| format!("{}: {e}", out_dir.display()))?;

    let js = out_dir.join("bundle.js");
    std::fs::write(&js, &bundled.code).map_err(|e| format!("{}: {e}", js.display()))?;

    if let Some(map) = &bundled.source_map {
        let p = out_dir.join("bundle.js.map");
        std::fs::write(&p, map).map_err(|e| format!("{}: {e}", p.display()))?;
    }

    // 能力声明单独落一份：Bundle::read 会读同目录的 capabilities.json。
    // 这份文件不声明，界面里每个 host.call 都会被拒。
    if let Some(caps) = &bundled.capabilities {
        let p = out_dir.join("capabilities.json");
        std::fs::write(&p, caps.to_json().to_string())
            .map_err(|e| format!("{}: {e}", p.display()))?;
    }

    println!("已生成 {}", js.display());
    Ok(())
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .to_path_buf()
}
