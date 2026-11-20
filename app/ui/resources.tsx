import { computed, effect, openUrl, Scroll, signal, Text, View, windowSize } from "nsh";
import { Dialog } from "nsh/base";
import { Button, card, colors, Detail, Heading, row } from "./controls";
import { activeTransfer, busy, report, send, type Component, type Integrity } from "./model";

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

const integrityLabel: Record<Integrity, string> = {
  missing: "待准备", quick: "快速自检通过", verified: "已通过完整核验", damaged: "版本损坏", conflict: "版本冲突",
};

export function writeIssue(item: Component): string {
  if (!item.writable) return "目标目录不可写";
  if (item.availableBytes !== null && item.requiredBytes > item.availableBytes) return "目标卷空间不足";
  return "";
}

export function ResourceHealth(props: { item: Component }) {
  const reviewed = signal<{ id: string; location: string; token: string } | null>(null);
  const changed = signal(false);
  const locked = computed(() => busy() || activeTransfer());
  const damaged = computed(() => props.item.integrity === "damaged");
  const confirming = computed(() => {
    const r = reviewed();
    return !!r && damaged() && r.id === props.item.id && r.location === props.item.location && r.token === props.item.repairToken;
  });
  const reason = computed(() => writeIssue(props.item));
  // 换盘、换目录或修复状态变化时，旧确认失效；即使原资源随后接回也必须重新查看。
  effect(() => {
    if (reviewed() && !confirming()) { reviewed.set(null); changed.set(true); }
  });
  function review() {
    const item = props.item;
    if (!damaged() || !item.repairToken || locked() || reason()) return;
    changed.set(false);
    reviewed.set({ id: item.id, location: item.location, token: item.repairToken });
  }
  function repair() {
    const original = reviewed();
    if (!original || locked() || !confirming() || reason()) return;
    reviewed.set(null);
    void send({ kind: "repairResource", resourceId: original.id, location: original.location,
      repairToken: original.token, acceptedLicenses: true, confirmed: true }, "重新安装 " + props.item.name);
  }
  return <View style={{ gap: 5 }}>
    <Location path={props.item.location} />
    <Detail id={"integrity-" + props.item.id} text={integrityLabel[props.item.integrity] + (props.item.ready && !props.item.writable ? " · 只读位置" : "")} />
    {props.item.issue ? <Text role="alert" style={{ userSelect: "text", fontSize: 13, color: colors().bad }}>{props.item.issue}</Text> : null}
    {!props.item.ready && props.item.integrity !== "conflict" ? <Detail text={"需额外空间 · " + size(props.item.requiredBytes) +
      (props.item.availableBytes === null ? "    可用空间未知" : "    可用 · " + size(props.item.availableBytes))} /> : null}
    {!props.item.ready && reason() ? <Detail text={reason()} /> : null}
    {damaged() && !props.item.repairToken ? <Detail text="尚未确认目标卷及目录身份，请重新检查。" /> :
      damaged() && changed() ? <Detail text="目标卷或版本状态已变化，请重新确认。" /> : null}
    <View style={row}>
      <LicenseLink item={props.item} />
      {props.item.integrity !== "missing" ? <Button id={"verify-" + props.item.id} label="完整核验" disabled={locked()}
        accessibilityLabel={"完整核验 " + props.item.name + " " + props.item.version}
        press={() => send({ kind: "verifyResource", resourceId: props.item.id }, "完整核验 " + props.item.name)} /> : null}
      {damaged() && !confirming() ? <Button id={"repair-" + props.item.id} label="重新安装此版本…" danger
        disabled={locked() || !!reason() || !props.item.repairToken} press={review} /> : null}
    </View>
    {confirming() ? <View testID={"repair-review-" + props.item.id} style={{ padding: 10, borderRadius: 8, gap: 6, backgroundColor: colors().warnBg }}>
      <Heading text={"重新安装 " + props.item.name + " · " + props.item.version} />
      <Detail text="将从官方来源准备此版本，完整核验后替换下面的目录。" />
      <Location path={props.item.location} />
      <View style={row}>
        <Button id={"confirm-repair-" + props.item.id} label="同意许可并重新安装" danger disabled={locked() || !!reason()} press={repair} />
        <Button id={"cancel-repair-" + props.item.id} label="取消" press={() => reviewed.set(null)} />
      </View>
    </View> : null}
  </View>;
}
