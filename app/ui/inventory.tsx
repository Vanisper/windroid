import { computed, effect, signal, Text, View, windowSize } from "nsh";
import { Button, card, colors, confirmation, Detail, Heading, row, Title } from "./controls";
import { HardwareFields, hardwareDraft, hardwareLimits, rendererName } from "./hardware";
import { DeviceControls } from "./library";
import { activeTransfer, busy, can, describeImage, environmentReady, images, instances, instanceState, managementBack, navigate, openEnvironment, openGuide, openInstance, openStorage, preparation, selected, send, snapshot, startCreation, switching, type Instance, type InstanceConfig } from "./model";

const unknown = "未知";
const cores = (n?: number | null) => n == null ? unknown : n + " 核";
function memory(mib?: number | null) {
  if (mib == null) return unknown;
  return mib % 1024 === 0 ? mib / 1024 + " GiB" : mib + " MiB";
}
function specs(item?: Instance | null) {
  const c = item?.config;
  return [
    { label: "CPU", value: cores(c?.cpuCores) },
    { label: "内存", value: memory(c?.ramMiB) },
    { label: "分辨率", value: c?.screenWidth && c.screenHeight ? c.screenWidth + " × " + c.screenHeight : unknown },
    { label: "用户空间", value: c?.diskBytes == null ? unknown : memory(c.diskBytes / 1024 ** 2) },
  ];
}

function NextAction() {
  const imageTransfer = computed(() => images().some(i => i.id === preparation()?.currentComponent) && activeTransfer());
  const label = computed(() => imageTransfer() ? "查看镜像进度" : activeTransfer() ? "查看准备进度" :
    environmentReady() ? "创建实例" : "查看运行环境");
  const next = () => {
    if (imageTransfer() && preparation()?.candidateTarget) openStorage();
    else if (imageTransfer()) openEnvironment("images");
    else if (environmentReady()) startCreation();
    else openEnvironment("components");
  };
  return <View style={{ gap: 12, alignItems: "flex-start" }}>
    <Detail text={imageTransfer() && preparation()?.candidateTarget ? "正在为候选目录准备镜像，完成后可回到存储设置确认切换。" :
      imageTransfer() ? "系统镜像正在准备，完成后可供实例使用。" : activeTransfer() ? "正在准备这台 Mac 共用的运行组件。" :
      environmentReady() ? "运行组件已就绪。选择系统镜像后即可创建实例。" : "先准备这台 Mac 共用的 Android 运行组件，再创建实例。"} />
    <View style={row}>
      <Button id="workspace-next" primary label={label()} press={next} />
      <Button id="guide-from-inventory" label="使用引导" press={() => openGuide("instances")} />
    </View>
  </View>;
}

function InventoryRow(props: { item: Instance }) {
  const stacked = computed(() => windowSize().width < 880);
  return <View testID={"row-" + props.item.id} style={{ flexDirection: stacked() ? "column" : "row", gap: 12, paddingVertical: 12,
    borderTopWidth: 1, borderColor: colors().line, alignItems: stacked() ? "stretch" : "center" }}>
    <View style={{ flexGrow: 1, flexBasis: stacked() ? undefined : 90, minWidth: 0, gap: 4 }}>
      <View testID={"enter-" + props.item.id} role="button" accessibilityLabel={"打开 " + props.item.name} tooltip={props.item.name}
        disabled={switching()} onPress={() => void openInstance(props.item.id, "library")}
        style={{ alignSelf: "flex-start", maxWidth: "100%", padding: 3, borderRadius: 4 }} focusVisibleStyle={{ backgroundColor: colors().selected }}>
        <Text style={{ fontSize: 15, fontWeight: 650, numberOfLines: 1, color: colors().accent }}>{props.item.name}</Text>
      </View>
      <Detail text={instanceState(props.item)} />
    </View>
    <View style={{ flexDirection: "row", flexWrap: "wrap", gap: 12, flex: stacked() ? undefined : 3, minWidth: 0 }}>
      <Each of={specs(props.item)} key={s => s.label}>{field => <View style={{ flexGrow: 1, flexBasis: 58, gap: 4 }}>
        <Text style={{ fontSize: 11, color: colors().muted }}>{field().label}</Text>
        <Text style={{ fontSize: 13, fontWeight: 550, color: colors().ink }}>{field().value}</Text>
      </View>}</Each>
    </View>
    <Button id={"manage-" + props.item.id} label="设置" accessibilityLabel={"管理 " + props.item.name} tooltip={"管理实例 · " + props.item.name}
      disabled={switching()} press={() => void openInstance(props.item.id, "manage")} />
  </View>;
}

export function Inventory() {
  return <View testID="inventory" style={{ gap: 18 }}>
    <Title text="全部实例" />
    {instances().length ? <View style={card()}>
      <Each of={instances()} key={item => item.id}>{item => <InventoryRow item={item()} />}</Each>
    </View> : <View style={card()}>
      <Heading text="还没有实例" />
      <NextAction />
    </View>}
  </View>;
}

function canEdit(item?: Instance | null) {
  return !!item && !item.processAlive && (item.state === "stopped" || item.state === "failed");
}

function EditHardware(props: {
  id: string; config: InstanceConfig; query: ReturnType<typeof hardwareLimits>; close: () => void;
}) {
  const id = props.id;
  const draft = hardwareDraft(props.query, props.config);
  async function save() {
    if (busy() || draft.issue() || id !== selected()?.id || !canEdit(selected())) return;
    if (await send({ kind: "updateHardware", instanceId: id, ...draft.values() }, "保存硬件规格")) props.close();
  }
  return <View testID="hardware-editor" style={{ gap: 12 }}>
    <HardwareFields draft={draft} />
    <Detail text="修改将在下次启动时生效。硬件变更后会跳过原快照，保留应用和文件。" />
    <View style={row}>
      <Button id="save-hardware" label="保存硬件规格" primary disabled={busy() || !!draft.issue()} press={save} />
      <Button id="cancel-hardware" label="取消" disabled={busy()} press={props.close} />
    </View>
  </View>;
}

export function ManageInstance() {
  const more = signal(false);
  const config = computed(() => selected()?.config);
  const editId = signal<string | null>(null);
  const limits = hardwareLimits(() => selected()?.config ? { instanceId: selected()!.id } : null);
  const expectedRam = computed(() => {
    const ram = config()?.ramMiB, floor = limits.limits()?.minRamMiB;
    return ram && floor ? Math.max(ram, floor) : null;
  });
  const editing = computed(() => editId() !== null && editId() === selected()?.id && canEdit(selected()));
  effect(() => { if (editId() && !editing()) editId.set(null); });
  const restartable = computed(() => can("launchApp") || selected()?.state === "stalled");
  function confirmRestart(cold: boolean) {
    const item = selected();
    if (!item || (!restartable() && !(cold && can("start")))) return;
    const label = cold ? "冷启动" : "重新启动";
    confirmation.set({ id: item.id, title: label + "「" + item.name + "」？", label,
      detail: cold ? "将跳过快照启动，保留已安装的应用和文件。" : "将结束当前会话，再启动此实例。",
      action: () => { void send({ kind: cold ? "coldRestart" : "restart", instanceId: item.id }, label); } });
  }
  const logPath = computed(() => {
    const home = snapshot()?.storage?.home;
    return home && selected() ? home + "/instances/" + selected()!.id + "/emulator.log" : null;
  });
  const observed = computed(() => selected()?.observed);
  return <View testID="manage-instance" style={{ gap: 16 }}>
    <Button id="manage-back" label={managementBack() === "library" ? "返回应用面板" : "返回全部实例"} press={() => navigate(managementBack())} />
    <Title text={selected()?.name ?? "管理实例"} />
    <View style={card()}>
      <Heading text="实例配置" />
      <Detail text={"设备模板 · " + (config()?.template ?? unknown)} />
      <Detail text={"系统镜像 · " + (config()?.image ? describeImage(config()!.image) : unknown)} />
      <Each of={specs(selected())} key={s => s.label}>{s => <Detail text={s().label + " · " + s().value} />}</Each>
      <Detail text={"图形渲染 · " + rendererName(config()?.graphics)} />
      {editing() && config() ? <EditHardware id={selected()!.id} config={config()!} query={limits} close={() => editId.set(null)} /> :
        canEdit(selected()) && config() ? <Button id="edit-hardware" label="调整硬件…" disabled={busy()} press={() => editId.set(selected()!.id)} /> :
        <Detail text={canEdit(selected()) ? "实例配置暂不可读取。" : "停止实例后可调整 CPU、内存和图形渲染。"} />}
    </View>
    <View style={card()}>
      <Heading text="本次运行" />
      {expectedRam() !== null ? <Detail text={"按当前配置预计分配内存 · " + memory(expectedRam())} /> : null}
      <Detail text={"观测到的 CPU · " + cores(observed()?.cpuCores) + "     内存 · " + memory(observed()?.ramMiB)} />
      {!environmentReady() ? <Button id="manage-environment" label="准备运行环境" press={() => navigate("environment")} /> : null}
    </View>
    <DeviceControls />
    <Button id="manage-more" label={more() ? "收起更多" : "更多"} press={() => more.set(!more())} />
    {more() ? <View style={card()}>
      <View style={row}>
        {restartable() ? <Button id="restart-instance" label="重新启动…" disabled={busy() || !environmentReady()} press={() => confirmRestart(false)} /> : null}
        {restartable() || can("start") ? <Button id="cold-restart" label="冷启动…" disabled={busy() || !environmentReady()} press={() => confirmRestart(true)} /> : null}
      </View>
      <Heading text="模拟器日志" />
      {logPath() ? <Text style={{ userSelect: "text", fontSize: 13, color: colors().ink }}>{logPath()}</Text> : null}
      <Detail text="每次启动模拟器都会覆盖上一次的日志。" />
    </View> : null}
  </View>;
}
