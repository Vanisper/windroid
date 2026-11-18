import { computed, effect, mount, Scroll, signal, Text, View, windowSize } from "nsh";
import { Button, colors, ConfirmDialog, Detail, Heading, Indicator, narrow, Title } from "./ui/controls";
import { CreateInstance } from "./ui/instances";
import { Library } from "./ui/library";
import { busy, connect, currentId, dropApk, instances, loadError, messageText, notice, pendingLabel, preparation, refresh, screen, selected, send, snapshot, instanceState, switching, type Message } from "./ui/model";
import { Environment } from "./ui/preparation";

function Sidebar() {
  const environmentReady = computed(() => !preparation() || preparation()?.phase === "ready");
  const contents = () => <View style={{ gap: 14, padding: narrow() ? 12 : 16 }}>
    <View style={{ flexDirection: "row", gap: 9, alignItems: "center" }}>
      <View style={{ height: 29, width: 29, borderRadius: 8, alignItems: "center", justifyContent: "center", backgroundColor: colors().accent }}>
        <Text style={{ color: colors().onAccent, fontSize: 17, fontWeight: 800 }}>W</Text>
      </View>
      <Text style={{ fontSize: 20, fontWeight: 700, color: colors().ink }}>Windroid</Text>
    </View>
    <View style={{ gap: 5 }}>
      <Heading text="实例" />
      <View style={{ flexDirection: narrow() ? "row" : "column", flexWrap: narrow() ? "wrap" : "nowrap", gap: 4 }}>
        <Each of={instances()} key={item => item.id}>{item => <View testID={"instance-" + item().id} role="button" accessibilityLabel={item().name + " · " + instanceState(item())}
          accessibilityState={{ selected: currentId() === item().id }} disabled={switching()}
          onPress={() => { screen.set("library"); if (currentId() !== item().id) void send({ kind: "selectInstance", instanceId: item().id }, "切换实例"); }}
          style={{ paddingHorizontal: 10, paddingVertical: 7, gap: 8, flexDirection: "row", alignItems: "center",
            borderRadius: 8, backgroundColor: item().id === currentId() ? colors().selected : colors().rail }} hoverStyle={{ backgroundColor: colors().wash }}>
          <Indicator state={item().state} />
          <View style={{ flexShrink: 1, gap: 2 }}>
            <Text style={{ fontSize: 14, fontWeight: 600, numberOfLines: 2, color: colors().ink }}>{item().name}</Text>
            <Text style={{ fontSize: 11, color: colors().muted }}>{instanceState(item())}</Text>
          </View>
        </View>}</Each>
        {!instances().length ? <Detail text="尚无实例" /> : null}
      </View>
    </View>
    <View style={{ flexDirection: narrow() ? "row" : "column", flexWrap: "wrap", gap: 8 }}>
      {environmentReady() && instances().length > 0 ?
        <Button id="new-instance" label="＋ 新建实例" disabled={busy()} press={() => screen.set("create")} /> : null}
      {preparation() ? <Button id="show-environment" label="运行环境" disabled={!environmentReady()} press={() => screen.set("environment")} /> : null}
    </View>
  </View>;
  return <Scroll testID="sidebar" style={{ width: narrow() ? "100%" : 194, maxHeight: narrow() ? Math.floor(windowSize().height * 0.38) : undefined,
    flexGrow: 0, flexShrink: 0, backgroundColor: colors().rail, borderBottomWidth: narrow() ? 1 : 0, borderRightWidth: narrow() ? 0 : 1, borderColor: colors().line }}>
    {contents()}
  </Scroll>;
}

function Feedback() {
  const visible = computed(() => pendingLabel() ? null : notice());
  const expanded = signal<Message | null>(null);
  effect(() => { if (expanded() && expanded() !== visible()) expanded.set(null); });

  return <View style={{ gap: 5 }}>
    {pendingLabel() ? <Detail id="pending" text={pendingLabel() + "…"} /> : null}
    <If when={visible()}>{note => <View style={{ gap: 5 }}>
      <Text testID="notice" role={note().failed ? "alert" : undefined} style={{ userSelect: "text", fontSize: 13, color: note().failed ? colors().bad : colors().accent }}>{messageText(note())}</Text>
      {note().installError?.detail ? <Button id="toggle-install-diagnostic" label={expanded() === note() ? "收起诊断" : "查看原始诊断"}
        press={() => expanded.set(expanded() === note() ? null : note())} /> : null}
      {expanded() === note() ? <Text testID="install-diagnostic" style={{ userSelect: "text", fontSize: 12, color: colors().muted }}>{note().installError?.detail}</Text> : null}
    </View>}</If>
    {snapshot()?.streamNote ? <Text testID="stream-note" style={{ userSelect: "text", fontSize: 13, color: colors().warn }}>{snapshot()?.streamNote}</Text> : null}
    {loadError() ? <View style={{ gap: 6 }}><Text style={{ fontSize: 13, color: colors().bad }}>{loadError()}</Text>
      <Button id="retry-snapshot" label="重试连接" press={refresh} /></View> : null}
  </View>;
}

function Workspace() {
  // 同一页面内只更新数据。进度与设备事件不应卸载表单或正在按下的按钮。
  const page = computed(() => {
    if (!snapshot()) return "loading";
    if (preparation() && (preparation()?.phase !== "ready" || screen() === "environment")) return "environment";
    if (!instances().length || screen() === "create") return "create";
    return selected() ? "library" : "choose";
  });
  return <View style={{ gap: 12, padding: narrow() ? 14 : 24, minWidth: 0 }}>
    <Feedback />
    {page() === "loading" ? <Title text={loadError() ? "无法连接应用库" : "正在读取应用库…"} /> :
      page() === "environment" ? <Environment /> : page() === "create" ? <CreateInstance /> :
      page() === "library" ? <Library /> : <Title text="选择一个实例" />}
  </View>;
}

function App() {
  connect();
  return <View testID="apk-drop" onFileDrop={e => dropApk(e.uris)} style={{ flex: 1, minWidth: 0, backgroundColor: colors().canvas }}>
    <View style={{ flex: 1, flexDirection: narrow() ? "column" : "row" }}>
      <Sidebar />
      <Scroll testID="workspace-scroll" style={{ flex: 1, minWidth: 0, minHeight: 0 }}><Workspace /></Scroll>
    </View>
    <ConfirmDialog />
  </View>;
}

mount(App);
