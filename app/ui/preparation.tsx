import { computed, openUrl, Scroll, signal, Text, View, windowSize } from "nsh";
import { Dialog } from "nsh/base";
import { Button, card, colors, Detail, Heading, row, Title } from "./controls";
import { busy, preparation, navigate, report, send, type Component, type Preparation } from "./model";

const phaseTitle: Record<Preparation["phase"], string> = {
  missing: "准备 Android 环境", downloading: "正在下载组件", verifying: "正在校验文件",
  installing: "正在安装组件", paused: "下载已暂停", failed: "环境准备失败", ready: "运行环境已就绪",
};
function size(bytes: number): string {
  if (!(bytes >= 0)) return "大小未知";
  if (bytes >= 1024 ** 3) return (bytes / 1024 ** 3).toFixed(2) + " GiB";
  if (bytes >= 1024 ** 2) return (bytes / 1024 ** 2).toFixed(1) + " MiB";
  return bytes >= 1024 ? (bytes / 1024).toFixed(0) + " KiB" : bytes + " B";
}
function sourceLabel(url: string) {
  try { return new URL(url).host || url; } catch { return url; }
}
const licenseId = signal<string | null>(null);

function LicenseDialog() {
  const component = computed(() => preparation()?.components.find(c => c.id === licenseId()));
  const close = () => licenseId.set(null);
  async function openSource() {
    const uri = component()?.licenseUrl;
    if (!uri?.startsWith("https://")) return;
    try {
      if (!(await openUrl(uri)).opened) throw new Error("系统未打开此网址");
    } catch (e) { close(); report("无法打开许可网页 · " + String(e), true); }
  }
  return <Dialog open={component() !== undefined} animation="fade" onCancel={close} accessibilityLabel="组件许可"
    backdropStyle={{ backgroundColor: "#0A170F70" }}
    style={{ ...card(), width: Math.min(560, windowSize().width - 32), height: Math.min(520, windowSize().height - 36) }}>
    <Heading text={component()?.license ?? "组件许可"} />
    <Detail text={(component()?.name ?? "") + " · " + (component()?.version ?? "")} />
    <Scroll testID="license-scroll" style={{ flex: 1 }} contentStyle={{ gap: 8 }}>
      <Text style={{ userSelect: "text", fontSize: 13, color: colors().ink }}>{component()?.licenseText || "完整条款可通过下方许可网址查阅。"}</Text>
      <Text style={{ userSelect: "text", fontSize: 12, color: colors().muted }}>{component()?.licenseUrl}</Text>
    </Scroll>
    <View style={row}>
      <Button id="close-license" label="完成" primary press={close} />
      {component()?.licenseUrl?.startsWith("https://") ? <Button id="license-source" label="打开许可网页" press={openSource} /> : null}
    </View>
  </Dialog>;
}

function ComponentRow(props: { item: Component }) {
  return <View style={{ paddingVertical: 10, borderTopWidth: 1, borderColor: colors().line, gap: 5 }}>
    <View style={row}>
      <Text style={{ flexGrow: 1, fontSize: 14, fontWeight: 600, color: colors().ink }}>{props.item.name}</Text>
      <Text style={{ fontSize: 12, color: props.item.ready ? colors().accent : colors().muted }}>{props.item.ready ? "已就绪" : size(props.item.sizeBytes)}</Text>
    </View>
    <Detail text={props.item.version + "  ·  " + sourceLabel(props.item.source)} />
    <View testID={"license-" + props.item.id} role="button" accessibilityLabel={"查看许可 · " + props.item.name}
      onPress={() => licenseId.set(props.item.id)} style={{ paddingVertical: 3, alignSelf: "flex-start" }}>
      <Text style={{ fontSize: 12, color: colors().accent }}>{props.item.license + "  ↗"}</Text>
    </View>
  </View>;
}

export function Environment() {
  const info = computed(() => preparation());
  const phase = computed(() => info()?.phase ?? "missing");
  const active = computed(() => ["downloading", "verifying", "installing"].includes(phase()));
  const percent = computed(() => {
    const p = info();
    return p && p.totalBytes > 0 ? Math.max(0, Math.min(100, Math.floor(p.downloadedBytes / p.totalBytes * 100))) : null;
  });
  const current = computed(() => info()?.components.find(c => c.id === info()?.currentComponent)?.name);
  const start = () => send({ kind: "prepareRuntime", acceptedLicenses: true }, phase() === "paused" ? "继续下载" : "准备运行环境");
  return <View testID="environment" style={{ gap: 18 }}>
    <Title text={phaseTitle[phase()]} />
    <Detail text={info()?.borrowed ? "正在使用指定的开发环境。更新组件后可重新检查。" : "组件保存在这台 Mac 上，所有实例共用。"} />
    <View style={card()}>
      <Heading text={phase() === "ready" ? "组件已就位" : "下载与安装"} />
      <Text testID="download-size" style={{ fontSize: 15, fontWeight: 600, color: colors().ink }}>
        {(info()?.totalBytes ?? 0) > 0 ? size(info()?.downloadedBytes ?? 0) + " / " + size(info()!.totalBytes) : "下载大小待确认"}
      </Text>
      {phase() !== "missing" && percent() !== null && phase() !== "ready" ?
        <View role="progressbar" accessibilityLabel={"下载进度 " + percent() + "%"} style={{ height: 5, borderRadius: 3, backgroundColor: colors().wash }}>
          <View style={{ width: (percent() + "%") as `${number}%`, height: 5, borderRadius: 3, backgroundColor: colors().accent }} />
        </View> : null}
      {current() && active() ? <Detail id="current-component" text={phaseTitle[phase()] + " · " + current()} /> : null}
      {info()?.error ? <Text testID="preparation-error" role="alert" style={{ userSelect: "text", fontSize: 13, color: colors().bad }}>{info()?.error}</Text> : null}
      {!info()?.borrowed && !active() && phase() !== "ready" ? <Detail text="开始或继续下载即表示同意下列组件各自的许可条款。" /> : null}
      <View style={row}>
        {phase() === "ready" ? <Button id="return-library" label="查看全部实例" primary press={() => navigate("instances")} /> : null}
        {!info()?.borrowed && !active() && phase() !== "ready" ?
          <Button id="prepare-runtime" primary disabled={busy() || !info()?.components.length} press={start}
            label={phase() === "paused" ? "同意并继续下载" : phase() === "failed" ? "同意并重试" : "同意并下载"} /> : null}
        {phase() === "downloading" ? <Button id="pause-preparation" label="暂停下载" disabled={busy()} press={() => send({ kind: "pausePreparation" }, "暂停下载")} /> : null}
        <Button id="recheck-runtime" label="重新检查" disabled={busy() || active()} press={() => send({ kind: "recheckRuntime" }, "检查环境")} />
      </View>
    </View>
    <View style={card()}>
      <Heading text="组件与许可" />
      <Each of={info()?.components} key={item => item.id}>{item => <ComponentRow item={item()} />}</Each>
    </View>
    <View style={card()}>
      <Heading text="系统镜像" />
      <Detail text="当前准备清单包含 Android 14 系统镜像。按版本选择和导入镜像将在后续版本提供。" />
    </View>
    <LicenseDialog />
  </View>;
}
