# Windroid

在自己的电脑上运行 Android 应用，每个应用以独立原生窗口出现。

当前进度：M0 骨架完成——窗口所有权、关闭拦截、设备域状态机、命令/事件/快照
通道、TSX 界面均已跑通。尚无真实模拟器接入。

## 文档入口

- [架构总览](docs/架构总览.md) — 系统如何切分、依赖方向、不可违反的约束、里程碑

各域设计文档与实测证据在 `docs/架构/` 与 `docs/实测/` 下，按依赖顺序编号。

## 代码结构

```
crates/contract   数据与纯函数：命令、事件、快照、状态推导
crates/device     设备域：单一写者 actor，副作用收在 Backend trait 之后
crates/host       呈现域：窗口所有权、关闭拦截、设备域到界面的桥
app/              应用入口与 TSX 界面
xtask/            构建期工具：生成类型声明、编译界面、类型检查
```

依赖方向是单向的：`app → host → device → contract`。契约不依赖任何 GUI 或平台
API，这是设备域将来可能拆成独立进程的前提。

## 构建与运行

```bash
cargo run -p xtask          # 生成类型、编译界面、类型检查
cargo run -p windroid       # 启动窗口
cargo test --workspace      # 全部测试
```

`app/.nsh/` 是生成物，不入库。首次检出后先跑一次 xtask：其中的类型声明
`.nsh/types` 决定了编辑器能否解析 `from "nsh"`，缺了会导致满屏报错而构建正常。

类型检查需要 `tsc`；本机没有时 xtask 只告警不失败，但界面里的类型错误
要等到运行期才暴露。
