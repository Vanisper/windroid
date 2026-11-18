# Windroid

在自己的电脑上运行 Android 应用，每个应用以独立原生窗口出现。

当前进度：M1a 完成——真实模拟器接入，真实 Android 画面（1920×1080）显示在
本项目自己的窗口里，随设备输入实时更新。协议层自研（非官方 client），
H.264 解码走 ffmpeg 独立进程。

## 文档入口

- [产品形态与交互](docs/产品/01-产品形态与交互.md) — 做成什么样：三个界面、关闭语义、资源治理
- [架构总览](docs/架构总览.md) — 系统如何切分、依赖方向、不可违反的约束、里程碑

各域设计文档与实测证据在 `docs/架构/` 与 `docs/实测/` 下，按依赖顺序编号。

## 代码结构

```
crates/contract   数据与纯函数：命令、事件、快照、状态推导、副作用边界
crates/adb        adb 调用封装：只执行与取输出，不判断成功
crates/runtime    运行环境域：实例目录、模拟器进程、adb 就绪判定
crates/device     设备域：单一写者 actor
crates/stream     流域：scrcpy 协议、视频包解析、H.264 解码
crates/host       呈现域：窗口所有权、关闭拦截、视频表面、设备域到界面的桥
app/              应用入口与 TSX 界面
```

依赖方向单向向下：`app → host → stream/device → runtime → adb → contract`。
契约不依赖任何 GUI 或平台 API，这是设备域将来可能拆成独立进程的前提。

`Backend`（副作用边界）定义在 `contract` 而非 `device`：真实实现在
`runtime`，位于设备域之下，trait 留在设备域会让依赖边倒着画。

## 构建与运行

```bash
nsh check app                   # 生成类型声明并类型检查
nsh build app -o app/.nsh/out   # 编译界面
cargo run -p windroid           # 启动窗口（假后端）
cargo test --workspace          # 全部测试
```

接真实模拟器：

```bash
# 需要 SDK、一个 AVD、以及 scrcpy-server jar
WINDROID_REAL=1 WINDROID_PORT=5554 cargo run -p windroid
```

可选环境变量：`WINDROID_AVD`（AVD 名）、`WINDROID_EMULATOR`、`WINDROID_ADB`、
`WINDROID_SERVER_JAR`、`WINDROID_HOME`、`WINDROID_GPU`（默认 `host`）。

`app/.nsh/` 是生成物，不入库。首次检出后先跑一次 `nsh check app`：其中的
类型声明 `.nsh/types` 决定了编辑器能否解析 `from "nsh"`，缺了会导致满屏
报错而构建本身正常。

界面构建用 nsh 官方 CLI，不自建构建工具。`nsh check` 自带 TypeScript 编译器
（首次运行自动下载并校验），无需 Node 或 `node_modules`。
