import { computed, effect, host, signal, Text, View } from "nsh";
import { Input } from "nsh/base";
import { Menu } from "nsh/desktop";
import { Button, colors, Detail, Heading, row } from "./controls";
import { busy, report, type GpuMode, type ImageIdentity, type InstanceConfig } from "./model";

interface Limits {
  maxCpuCores: number; minRamMiB: number; maxRamMiB: number;
  defaultCpuCores: number; defaultRamMiB: number;
  maxDiskGiB: number | null; defaultDiskGiB: number | null;
}
type Target = { instanceId: string } | { template: string; imageId: string; image: ImageIdentity };

export function hardwareLimits(target: () => Target | null) {
  const key = computed(() => JSON.stringify(target()));
  const limits = signal<Limits | null>(null);
  const error = signal("");
  const revision = signal(0);
  effect(() => {
    const request = JSON.parse(key()) as Target | null;
    revision();
    limits.set(null);
    error.set("");
    if (!request) return;
    // 下载进度等快照变化不重发查询；更换模板、镜像修订或离开页面时丢弃旧回执。
    let active = true;
    void host.call<Limits>("windroid.hardwareLimits", request).then(
      result => { if (active) limits.set(result); },
      reason => { if (active) error.set(String(reason)); },
    );
    return () => { active = false; };
  });
  return { limits, error, retry: () => revision.update(n => n + 1) };
}

const renderers: Record<GpuMode, string> = {
  host: "硬件加速", batch: "硬件加速（Vulkan 批处理）", auto: "模拟器自动选择", software: "软件渲染",
};
export const rendererName = (mode?: GpuMode | null) => mode ? renderers[mode] ?? "未知" : "未知";
const displayRam = (mib: number) => mib % 512 === 0 ? mib / 1024 + " GiB" : mib + " MiB";

function quantity(initial: () => number | null | undefined) {
  const override = signal<string | null>(null);
  const text = computed(() => override() ?? String(initial() ?? ""));
  const amount = computed(() => /^\d+$/.test(text()) ? Number(text()) : NaN);
  return { text, amount, set: (value: string) => override.set(value) };
}
type Quantity = ReturnType<typeof quantity>;

export function hardwareDraft(query: ReturnType<typeof hardwareLimits>, saved?: InstanceConfig) {
  const bounds = query.limits;
  const cpu = quantity(() => saved ? saved.cpuCores : bounds()?.defaultCpuCores);
  const ram = quantity(() => saved ? saved.ramMiB : bounds()?.defaultRamMiB);
  const disk = quantity(() => bounds()?.defaultDiskGiB);
  const graphics = signal<GpuMode>(saved?.graphics ?? "host");
  const valid = (n: number, min: number, max: number) => Number.isSafeInteger(n) && n >= min && n <= max;
  const issue = computed(() => {
    const lim = bounds();
    if (!lim) return query.error() || "正在读取设备和数据卷规格…";
    if (lim.maxRamMiB < lim.minRamMiB) return "此配置至少需要 " + displayRam(lim.minRamMiB) + " 内存，本机单实例最多可分配 " + displayRam(lim.maxRamMiB) + "。";
    if (!valid(cpu.amount(), 1, lim.maxCpuCores)) return "请输入 1 至 " + lim.maxCpuCores + " 的整数核数。";
    const inheritedRam = saved && ram.amount() > 0 && ram.amount() === saved.ramMiB && ram.amount() < lim.minRamMiB;
    if (!inheritedRam && !valid(ram.amount(), lim.minRamMiB, lim.maxRamMiB)) return "请输入 " + lim.minRamMiB + " 至 " + lim.maxRamMiB + " MiB 的整数内存。";
    if (!saved) {
      if (lim.maxDiskGiB === null || lim.defaultDiskGiB === null || lim.maxDiskGiB < 6) return "实例数据卷可分配的用户空间不足 6 GiB，请释放空间后重新检查。";
      if (!valid(disk.amount(), 6, lim.maxDiskGiB)) return "用户空间可设为 6 至 " + lim.maxDiskGiB + " GiB 的整数。";
    }
    return "";
  });
  const values = () => ({ cpuCores: cpu.amount(), ramMiB: ram.amount(), gpuMode: graphics() });
  return { query, cpu, ram, disk, graphics, issue, values, saved };
}

function NumberField(props: {
  id: string; label: string; unit: string; field: Quantity; min: number; max: number;
  presets: number[]; format: (n: number) => string;
}) {
  const custom = signal(false);
  function menu() {
    const options = Array.from(new Set([props.min, ...props.presets])).filter(n => n >= props.min && n <= props.max).sort((a, b) => a - b);
    void Menu.popup([
      ...options.map(n => ({ label: props.format(n), checked: !custom() && props.field.amount() === n,
        onSelect: () => { props.field.set(String(n)); custom.set(false); } })),
      { label: "自定义…", checked: custom(), onSelect: () => custom.set(true) },
    ]).catch(e => report("无法显示" + props.label + "选项 · " + String(e), true));
  }
  return <View style={{ gap: 5 }}>
    <Heading text={props.label} />
    <View style={row}>
      <Button id={"choose-" + props.id} label={custom() ? "自定义…" : Number.isSafeInteger(props.field.amount()) ? props.format(props.field.amount()) + "  ▾" : "选择…"}
        accessibilityLabel={"选择" + props.label} disabled={busy() || props.max < props.min} press={menu} />
      {custom() ? <Input testID={"custom-" + props.id} accessibilityLabel={"自定义" + props.label + "（" + props.unit + "）"}
        value={props.field.text()} onChange={e => props.field.set(e.value)} autoCorrect={false} disabled={busy()}
        style={{ width: 92, minHeight: 32, paddingHorizontal: 8, borderWidth: 1, borderRadius: 6, borderColor: colors().line, color: colors().ink, backgroundColor: colors().canvas }} /> : null}
      {custom() ? <Detail text={props.unit} /> : null}
      {props.max >= props.min ? <Detail text={props.min + "–" + props.max + " " + props.unit} /> : null}
    </View>
  </View>;
}

export function HardwareFields(props: { draft: ReturnType<typeof hardwareDraft> }) {
  const draft = props.draft;
  const limits = draft.query.limits;
  const chooseGraphics = () => Menu.popup((Object.keys(renderers) as GpuMode[]).map(mode => ({
    label: renderers[mode], checked: draft.graphics() === mode, onSelect: () => draft.graphics.set(mode),
  }))).catch(e => report("无法显示图形渲染选项 · " + String(e), true));
  return <View style={{ gap: 10 }}>
    <If when={limits()}>{lim => <View testID="hardware-fields" style={{ gap: 12 }}>
      <NumberField id="cpu" label="CPU" field={draft.cpu} unit="核" min={1} max={lim().maxCpuCores}
        presets={[2, 4, 6, 8, 12, 16, 24, 32, 48, 64]} format={n => n + " 核"} />
      <NumberField id="ram" label="内存" field={draft.ram} unit="MiB" min={lim().minRamMiB} max={lim().maxRamMiB}
        presets={[2048, 3072, 4096, 6144, 8192, 12288, 16384, 24576, 32768, 65536]} format={displayRam} />
      {draft.saved && draft.ram.amount() > 0 && draft.ram.amount() < lim().minRamMiB && draft.ram.amount() === draft.saved.ramMiB ?
        <Detail text={"保留原设 " + displayRam(draft.ram.amount()) + "，预计启动分配 " + displayRam(lim().minRamMiB) + "。"} /> : null}
      <Heading text="图形渲染" />
      <Button id="choose-graphics" label={rendererName(draft.graphics()) + "  ▾"} disabled={busy()} press={chooseGraphics} />
      {!draft.saved ? <View style={{ gap: 4 }}>
        <NumberField id="disk" label="用户空间" field={draft.disk} unit="GiB" min={6} max={lim().maxDiskGiB ?? 0}
          presets={[6, 8, 16, 32]} format={n => n + " GiB"} />
        <Detail text="容量上限取实例数据卷当前可用空间的 70%，创建后固定。" />
      </View> : null}
    </View>}</If>
    {draft.issue() ? <Text testID="hardware-issue" role="alert" style={{ fontSize: 13, color: colors().warn }}>{draft.issue()}</Text> : null}
    {draft.query.error() || (!draft.saved && limits() && draft.issue()) ?
      <Button id="retry-hardware" label="重新读取规格" press={draft.query.retry} /> : null}
  </View>;
}
