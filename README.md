# Windroid

Windroid 让 Android 应用以独立的原生窗口运行在桌面上。用户从应用库管理实例、安装 APK 和打开应用，也可以单独打开设备视图，操作同一实例的 Android 主屏。

## 平台进度

| 平台 | 开发进度 |
|---|---|
| macOS（Apple 芯片） | 首个可用版本。托管运行环境、实例隔离和独立应用窗口均已在 M4 上实测。 |
| Linux | 下一个桌面目标，兼顾客厅电视场景。需要提供 Linux 运行时组件，验证 GTK4 窗口、视频解码和输入。 |
| Windows | 第三个桌面目标。需要适配运行时组件、进程和文件管理，验证 Win32 窗口、视频解码和输入。 |

实例状态机、应用命令和 scrcpy 协议属于共享核心。桌面端按平台接入模拟器、解码器和原生窗口。[产品形态](docs/产品/01-产品形态与交互.md) · [平台适配与验收](docs/架构/04-呈现域.md#七跨平台表面策略)

## 首次使用（macOS）

Windroid 默认打开 `~/.windroid`。如果目录无法打开，启动窗口会显示实际路径和原因。点「选择其他目录…」可打开已有工作区，也可选择空文件夹新建工作区。所选目录用于本次运行。

1. 打开 `Windroid.app`。新工作区从「全部实例」开始。可从「运行环境」分别准备共享组件和系统镜像，也可从「设置 → 引导」逐步准备。下载前可查看版本、大小、存储位置和许可，中断后可在原处手动继续。
2. 组件就绪后，新建实例，选用本机镜像、下载另一版或导入清单匹配的官方完整 ZIP。选择设备模板、名称、CPU、内存、图形渲染和用户空间后创建。启动时检查本机核心数、共享内存额度、实时余量和端口。侧栏的「实例」显示全部实例的配置与状态，可对整表或勾选的实例批量启停。点名称进入应用面板，点行内「设置」可在停止后调整 CPU、内存与渲染。重开工作区会恢复最近选择的有效实例。
3. 等实例就绪后，从应用库打开已安装的应用或安装 APK。点击「设备视图」可打开主屏，每个应用窗口有自己的 Android 显示和输入通道。再次打开同一实例的同一应用会聚焦原窗口。
4. 关闭应用窗口时选择「保留后台」「完全退出」或「取消」，前两项可记为所有应用窗口的默认选择。在该实例的「运行中」点「恢复窗口」，可再次打开 Android 仍保留的任务。关闭设备窗口只结束该路投屏。

在「设置 → 存储位置」可分别选择运行组件和系统镜像的文件夹。选已有资源时会按官方展开清单核验，符合的版本可直接共用。实例绑定具体镜像修订，自己的应用、数据和快照随工作区保存。首次打开只有预放资源的工作区会先展示核验结果，确认后建库。[目录布局与接入](docs/架构/01-运行环境域.md#四目录布局)

## 开发命令

在仓库根目录运行 `npm run` 可列出所有命令，也可用 `pnpm run <脚本名>`。装有 Node.js 和 npm 即可使用脚本入口，各命令直接调用相应的开发工具。[命令清单](package.json)

| 命令 | 用途 |
|---|---|
| `npm run check` / `npm test` | 编译检查、测试四个共享 Rust 模块，涵盖契约、设备状态、adb 和 scrcpy 协议。开发这些模块只需 Rust 1.97+ 和宿主编译工具链。 |
| `npm run check:ui` / `npm run build:ui` | 检查 TSX 类型、构建开发界面。需要 nsh 0.5.x CLI，首次类型检查会下载并校验 TypeScript 编译器。 |
| `npm run dev:macos` | 构建界面并从源码启动应用库。 |
| `npm run check:macos` / `npm run test:macos` | 检查界面和整个 Rust 工作区（Clippy），或运行整个工作区的测试。 |
| `npm run package:macos` | 构建 release、打包界面并签名，默认输出 `dist/Windroid.app`。 |
| `npm run fixtures:apk` | 构建 Alpha、Beta 两个签名测试 APK，默认输出到 `build/test-apks/`。需要 Bash、JDK 和 Android SDK 的 API 34 平台及构建工具。 |

完整 macOS 构建需要 Apple 芯片的 Mac、Xcode 命令行工具、Rust、nsh CLI 和 Python 3。运行 `open dist/Windroid.app` 可打开打好的应用。两个打包命令均接受自定义输出目录，如 `npm run package:macos -- "./build/mac app"`。[Mac 打包脚本](scripts/package-macos.sh) · [测试 APK 构建脚本](scripts/build-test-apks.sh)

开发启动和打包启动默认都使用真实后端。开发时可通过环境变量切换数据位置或使用测试模式。

| 变量 | 用途 |
|---|---|
| `WINDROID_HOME` | 指定启动时首先打开的工作区绝对路径，默认为 `~/.windroid`。 |
| `WINDROID_SHOW_GUIDE_ON_FIRST_LAUNCH=1` | 启用产品首开欢迎页，仅作用于从未展示过首个页面的新工作区。 |
| `WINDROID_FAKE=1` | 启用用于开发与测试的假后端。 |

## 实现与验证

Rust 负责实例进程、每实例的设备状态、scrcpy 5.0.1 协议和 macOS 窗口。H.264 通过系统 VideoToolbox 解码为 BGRA 帧。TSX/nsh 实现应用库、运行环境准备和实例管理界面。

四个共享模块已在 Linux ARM64 容器中通过 `npm test`。从 macOS 运行 `npm run check` 并指定目标，也已通过 Linux x86_64 和 Windows x86_64（MSVC）的交叉编译检查。GitHub Actions 工作流为 Linux、Windows 和 macOS 配置了同一条 `npm test` 命令。adb 进程与协议回环的集成用例使用 Unix 夹具，像素解码用例使用 macOS VideoToolbox。[核心测试工作流](.github/workflows/core.yml)

macOS 上的工作区测试覆盖实例状态、APK 安装、双显示协议、VideoToolbox 像素解码、窗口回收和应用库交互。需要真实模拟器的用例单独运行。[工作区保护与版本启动](docs/实测/06-工作区保护与版本启动.md)记录原生首次打开、双窗口及跨卷迁移的复现方法。其他实测包含[私有实例的冷启动与快照恢复](docs/实测/01-运行环境域.md)、[主屏与应用窗口的尺寸和输入](docs/实测/03-流域.md)，以及[从应用库安装、切换桌面和关窗](docs/实测/04-呈现域.md)。

- [产品形态与交互](docs/产品/01-产品形态与交互.md)
- macOS 桌面设计：[可交互原型](output/design/Windroid-Prototype.html)可下载后用浏览器打开，从「原型导览」选择场景；[可编辑设计稿](output/design/Windroid-macOS.pen)保留各状态的布局。原生客户端的使用方式见上文。
- [架构总览与各域索引](docs/架构总览.md)
