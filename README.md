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
xtask/            构建期工具：把 App.tsx 编译成 bundle.js
```

依赖方向是单向的：`app → host → device → contract`。契约不依赖任何 GUI 或平台
API，这是设备域将来可能拆成独立进程的前提。

## 构建与运行

```bash
cargo run -p xtask          # 编译界面，产出 app/bundle.js
cargo run -p windroid       # 启动窗口
cargo test --workspace      # 全部测试
```

界面产物不入库，改动 `app/App.tsx` 后需重新运行 xtask。
