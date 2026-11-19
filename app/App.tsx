import { computed, effect, mount, Scroll, signal, Text, View } from "nsh";
import { Button, colors, ConfirmDialog, Detail, narrow, Title } from "./ui/controls";
import { CreateInstance } from "./ui/instances";
import { Guide } from "./ui/guide";
import { Inventory, ManageInstance } from "./ui/inventory";
import { Library } from "./ui/library";
import { connect, currentId, firstPageShown, instancePage, loadError, messageText, notice, openInstance, pendingLabel, refresh, screen, snapshot, type Message } from "./ui/model";
import { Environment } from "./ui/preparation";
import { LicenseDialog } from "./ui/resources";
import { Settings } from "./ui/settings";
import { Sidebar } from "./ui/sidebar";

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
    {instancePage() && snapshot()?.streamNote ? <Text testID="stream-note" style={{ userSelect: "text", fontSize: 13, color: colors().warn }}>{snapshot()?.streamNote}</Text> : null}
    {loadError() ? <View style={{ gap: 6 }}><Text style={{ fontSize: 13, color: colors().bad }}>{loadError()}</Text>
      <Button id="retry-snapshot" label="重试连接" press={refresh} /></View> : null}
  </View>;
}

function Workspace() {
  // 页面只响应导航。下载进度与设备事件通过快照更新当前页面。
  const page = computed(() => snapshot() ? screen() : "loading");
  return <View style={{ flex: 1, minHeight: 0, minWidth: 0 }}>
    {page() === "settings" ? <View onLayout={firstPageShown} style={{ flex: 1, minHeight: 0, gap: 12, padding: narrow() ? 14 : 24 }}>
      <Feedback /><Settings />
    </View> : <Scroll testID="workspace-scroll" style={{ flex: 1, minWidth: 0, minHeight: 0 }}
      contentStyle={{ gap: 12, padding: narrow() ? 14 : 24 }}>
      <Feedback />
      {page() === "loading" ? <Title text={loadError() ? "无法连接应用库" : "正在读取应用库…"} /> :
        <View onLayout={firstPageShown} style={{ minWidth: 0 }}>
          {page() === "environment" ? <Environment /> : page() === "create" ? <CreateInstance /> :
            page() === "guide" ? <Guide /> : page() === "manage" ? <ManageInstance /> : page() === "library" ?
              <Library onManage={() => { const id = currentId(); if (id) void openInstance(id, "manage"); }} /> : <Inventory />}
        </View>}
    </Scroll>}
  </View>;
}

function App() {
  connect();
  return <View style={{ flex: 1, minWidth: 0, backgroundColor: colors().canvas }}>
    <View style={{ flex: 1, flexDirection: "row" }}>
      <Sidebar />
      <Workspace />
    </View>
    <ConfirmDialog />
    <LicenseDialog />
  </View>;
}

mount(App);
