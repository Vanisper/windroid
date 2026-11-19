import { computed, signal, Text, View, windowSize } from "nsh";
import { Button, card, colors, Detail, Heading, row, Title } from "./controls";
import { DeviceControls } from "./library";
import { activeTransfer, describeImage, environmentReady, images, instances, instanceState, managementBack, navigate, openEnvironment, openGuide, openInstance, openStorage, preparation, selected, snapshot, startCreation, switching, type Instance } from "./model";

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
    { label: "主屏尺寸", value: c?.screenWidth && c.screenHeight ? c.screenWidth + " × " + c.screenHeight : unknown },
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
    <Detail text={imageTransfer() ? "系统镜像正在准备，完成后可供实例使用。" : activeTransfer() ? "正在准备这台 Mac 共用的运行组件。" :
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
      <Detail text="批量启停将在后续版本提供。当前可在各实例的管理页启停。" />
    </View> : <View style={card()}>
      <Heading text="还没有实例" />
      <NextAction />
    </View>}
  </View>;
}

export function ManageInstance() {
  const more = signal(false);
  const config = computed(() => selected()?.config);
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
      <Detail text="自定义硬件配置将在后续版本提供。" />
    </View>
    <View style={card()}>
      <Heading text="本次运行" />
      <Detail text={"观测到的 CPU · " + cores(observed()?.cpuCores) + "     内存 · " + memory(observed()?.ramMiB)} />
      {!environmentReady() ? <Button id="manage-environment" label="准备运行环境" press={() => navigate("environment")} /> : null}
    </View>
    <DeviceControls />
    <Button id="manage-more" label={more() ? "收起更多" : "更多"} press={() => more.set(!more())} />
    {more() ? <View style={card()}>
      <Heading text="模拟器日志" />
      {logPath() ? <Text style={{ userSelect: "text", fontSize: 13, color: colors().ink }}>{logPath()}</Text> : null}
      <Detail text="启动模拟器时会写入本次输出。按日期和启动记录浏览历史将在后续版本提供。" />
    </View> : null}
  </View>;
}
