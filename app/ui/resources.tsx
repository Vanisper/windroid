import { computed, openUrl, Scroll, signal, Text, View, windowSize } from "nsh";
import { Dialog } from "nsh/base";
import { Button, card, colors, Detail, Heading, row } from "./controls";
import { report, type Component } from "./model";

export function size(bytes: number): string {
  if (!(bytes >= 0)) return "大小未知";
  if (bytes >= 1024 ** 3) return (bytes / 1024 ** 3).toFixed(2) + " GiB";
  if (bytes >= 1024 ** 2) return (bytes / 1024 ** 2).toFixed(1) + " MiB";
  return bytes >= 1024 ? (bytes / 1024).toFixed(0) + " KiB" : bytes + " B";
}

const license = signal<Component | null>(null);
export function LicenseLink(props: { item: Component; id?: string }) {
  return <Button id={props.id ?? "license-" + props.item.id} label={props.item.license + "  ↗"}
    accessibilityLabel={"查看许可 · " + props.item.name} press={() => license.set(props.item)} />;
}

export function LicenseDialog() {
  const close = () => license.set(null);
  async function openSource() {
    const uri = license()?.licenseUrl;
    if (!uri?.startsWith("https://")) return;
    try {
      if (!(await openUrl(uri)).opened) throw new Error("系统未打开此网址");
    } catch (e) { close(); report("无法打开许可网页 · " + String(e), true); }
  }
  return <Dialog open={license() !== null} animation="fade" onCancel={close} accessibilityLabel="资源许可"
    backdropStyle={{ backgroundColor: "#0A170F70" }}
    style={{ ...card(), width: Math.min(560, windowSize().width - 32), height: Math.min(520, windowSize().height - 36) }}>
    <Heading text={license()?.license ?? "许可"} />
    <Detail text={(license()?.name ?? "") + " · " + (license()?.version ?? "")} />
    <Scroll testID="license-scroll" style={{ flex: 1 }} contentStyle={{ gap: 8 }}>
      <Text style={{ userSelect: "text", fontSize: 13, color: colors().ink }}>{license()?.licenseText || "完整条款可通过下方许可网址查阅。"}</Text>
      <Text style={{ userSelect: "text", fontSize: 12, color: colors().muted }}>{license()?.licenseUrl}</Text>
    </Scroll>
    <View style={row}>
      <Button id="close-license" label="完成" primary press={close} />
      {license()?.licenseUrl?.startsWith("https://") ? <Button id="license-source" label="打开许可网页" press={openSource} /> : null}
    </View>
  </Dialog>;
}

export function Progress(props: { id: string; downloaded: number; total: number }) {
  const percent = computed(() => props.total > 0 ? Math.max(0, Math.min(100, Math.floor(props.downloaded / props.total * 100))) : 0);
  return <View style={{ gap: 6 }}>
    <Detail text={size(props.downloaded) + " / " + size(props.total)} />
    <View testID={props.id} role="progressbar" accessibilityLabel={"下载进度 " + percent() + "%"}
      style={{ height: 5, borderRadius: 3, backgroundColor: colors().wash }}>
      <View style={{ width: (percent() + "%") as `${number}%`, height: 5, borderRadius: 3, backgroundColor: colors().accent }} />
    </View>
  </View>;
}

export function Location(props: { path: string }) {
  return <Text style={{ userSelect: "text", fontSize: 12, color: colors().muted }}>{props.path}</Text>;
}
