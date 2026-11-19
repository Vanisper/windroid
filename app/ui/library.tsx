import { computed, effect, Scroll, signal, Text, View, windowSize } from "nsh";
import { Input, Tab, Tabs } from "nsh/base";
import { Menu, type MenuItem } from "nsh/desktop";
import { Button, card, colors, confirmation, Detail, Heading, Indicator, row, Title } from "./controls";
import { busy, can, currentId, dropApk, report, running, selected, send, snapshot, switching, type Action, type AppItem, type RunningApp } from "./model";

const points: Record<string, string> = { process: "模拟器进程", adb: "设备连接", activity: "activity 服务", package: "package 服务", window: "window 服务" };

function DesktopPicker() {
  const desktop = computed(() => snapshot()?.desktop);
  const label = (pkg: string) => desktop()?.candidateLabels?.[pkg] ?? groups().flatMap(g => g.apps).find(a => a.package === pkg)?.label ?? pkg;
  const choose = () => {
    const id = currentId();
    if (!id || !can("launchApp")) return;
    const options = (desktop()?.candidates ?? []).map(pkg => ({ label: label(pkg), checked: pkg === desktop()?.defaultPackage,
      onSelect: () => { if (currentId() === id) void send({ kind: "setDefaultHome", instanceId: id, package: pkg }, "设置默认桌面"); } }));
    void Menu.popup(options).catch(e => report("无法显示桌面列表 · " + String(e), true, id));
  };
  // HOME 候选可能晚于首帧到达，也会随当前实例改变。
  return <If when={(desktop()?.candidates?.length ?? 0) > 0}>
    <View style={row}>
      <Detail text={"默认桌面 · " + (desktop()?.defaultPackage ? label(desktop()!.defaultPackage!) : "尚未设置")} />
      <Button id="choose-desktop" label="选择桌面…" disabled={busy() || !can("launchApp")} press={choose} />
    </View>
  </If>;
}

function DeviceStatus() {
  const services = computed(() => {
    const facts = snapshot()?.facts;
    if (!facts?.services) return "";
    const { activity, package: packages, window } = facts.services;
    return "服务 " + (Number(activity) + Number(packages) + Number(window)) + "/3 · adb " + (facts.adbVisible ? "已连接" : "未连接");
  });
  return <View style={{ gap: 6 }}>
    <View style={row}>
      <Indicator state={snapshot()?.state ?? "absent"} />
      <Text testID="device-summary" style={{ flexGrow: 1, color: colors().ink, fontSize: 14, fontWeight: 600 }}>{snapshot()?.frozen ? "已冻结 · 应用与文件已保存" : snapshot()?.summary}</Text>
      {snapshot()?.state !== "stopped" ? <Detail id="services" text={services()} /> : null}
    </View>
    {snapshot()?.stuckAt ? <Detail id="stuck" text={"等待" + (points[snapshot()!.stuckAt!] ?? snapshot()?.stuckAt)} /> : null}
  </View>;
}

function LifecycleButtons() {
  const startable = computed(() => selected()?.state === "stopped" || selected()?.state === "failed");
  return <View style={row}>
    {startable() ? <Button id="start" label={snapshot()?.frozen ? "恢复实例" : snapshot()?.state === "failed" ? "重新启动" : "启动实例"}
      primary disabled={busy() || !can("start")} tooltip={can("start") ? undefined : "请先准备运行环境"} press={() => act("start", "启动实例")} /> : null}
    {can("stop") ? <Button id="stop" label="停止实例" disabled={busy()} press={() => act("stop", "停止实例")} /> : null}
  </View>;
}

export function DeviceControls() {
  const remove = () => {
    const instance = selected();
    if (!instance || !can("delete")) return;
    confirmation.set({ id: instance.id, title: "删除「" + instance.name + "」？", label: "删除实例",
      detail: "此实例的应用和用户文件将一并删除。",
      action: () => { void send({ kind: "deleteInstance", instanceId: instance.id }, "删除实例"); } });
  };
  return <View style={card()}>
    <DeviceStatus />
    <View style={row}>
      <LifecycleButtons />
      {can("delete") ? <Button id="delete-instance" label="删除实例…" danger disabled={busy()} press={remove} /> : null}
    </View>
    <DesktopPicker />
  </View>;
}

function act(kind: "start" | "stop" | "openDevice" | "addApk" | "refreshApps", label: string) {
  const instanceId = currentId();
  if (instanceId) void send({ kind, instanceId }, label);
}

function RunningRow(props: { item: RunningApp }) {
  const dispatch = (kind: "focusApp" | "backgroundApp" | "quitApp", label: string) => {
    const app = props.item;
    void send({ kind, instanceId: app.instanceId, package: app.package }, label + " · " + app.label);
  };
  return <View style={{ ...row, paddingVertical: 8, borderTopWidth: 1, borderColor: colors().line }}>
    <View style={{ flexGrow: 1, flexBasis: 150, gap: 2 }}>
      <Text style={{ color: colors().ink, fontSize: 14, fontWeight: 600 }}>{props.item.label || props.item.package}</Text>
      <Detail text={props.item.windowOpen ? "窗口已打开" : "后台运行"} />
    </View>
    <Button id={"focus-" + props.item.package} label={props.item.windowOpen ? "聚焦窗口" : "恢复窗口"}
      accessibilityLabel={(props.item.windowOpen ? "聚焦 " : "恢复 ") + props.item.label + " 窗口"}
      disabled={busy() || !can("launchApp")} press={() => dispatch("focusApp", "显示窗口")} />
    {props.item.windowOpen ? <Button id={"background-" + props.item.package} label="转入后台" accessibilityLabel={"将 " + props.item.label + " 转入后台"}
      disabled={busy() || !can("launchApp")} press={() => dispatch("backgroundApp", "转入后台")} /> : null}
    <Button id={"quit-" + props.item.package} label="退出应用" accessibilityLabel={"退出 " + props.item.label} danger disabled={busy() || !can("launchApp")} press={() => dispatch("quitApp", "退出应用")} />
  </View>;
}

const tasksKnown = computed(() => snapshot()?.tasksLoaded === true && !snapshot()?.runningError);

function RunningList() {
  return <View testID="running-list" style={{ gap: 8 }}>
    {snapshot()?.runningError ? <Text testID="running-error" role="alert" style={{ fontSize: 13, color: colors().bad }}>{"无法更新运行状态 · " + snapshot()?.runningError}</Text> :
      !tasksKnown() ? <Detail id="tasks-pending" text={can("launchApp") ? "正在读取运行中的应用…" : "实例就绪后将读取运行任务。"} /> : null}
    <Each of={running()} key={item => item.instanceId + ":" + item.package}>{item => <RunningRow item={item()} />}</Each>
    {tasksKnown() && !running().length ? <Detail id="tasks-empty" text="此实例没有运行中的应用" /> : null}
  </View>;
}

function runAction(id: string, app: AppItem, action: Action) {
  if (!action.enabled || busy() || currentId() !== id) return;
  const run = () => { void send({ kind: action.kind, instanceId: id, package: app.package }, action.label + " · " + app.label); };
  if (action.destructive) {
    confirmation.set({ id, title: action.label + " · " + app.label, label: action.label,
      detail: "此应用在当前实例中的账号、设置和本地文件将被清除。", action: run });
  } else run();
}

function AppTile(props: { app: AppItem; instanceId: string }) {
  const launch = computed(() => props.app.actions.find(a => a.kind === "launch"));
  const enabled = computed(() => launch()?.enabled === true && can("launchApp") && !busy());
  function menu(at?: { x: number; y: number }) {
    // 原生菜单可在一次快照刷新后才完成选择。保留打开时的实例与包名。
    const app = props.app, id = props.instanceId;
    const items: MenuItem[] = app.actions.map(action => ({
      label: action.enabled || !action.reason ? action.label : action.label + " · " + action.reason,
      disabled: !action.enabled || busy(), onSelect: () => runAction(id, app, action),
    }));
    const custom = groups().filter(g => g.id.startsWith("group-"));
    if (custom.length) {
      const membership = custom.find(g => g.apps.some(a => a.package === app.package))?.id ?? "";
      const move = (groupId: string) => {
        if (currentId() === id) void send({ kind: "moveApp", instanceId: id, package: app.package, groupId }, "移动应用");
      };
      items.push({ type: "separator" });
      for (const g of [{ id: "", name: "原始分组" }, ...custom]) {
        items.push({ label: "移至" + g.name, checked: membership === g.id, disabled: membership === g.id || busy(), onSelect: () => move(g.id) });
      }
    }
    void Menu.popup(items, at).catch(e => report("无法打开应用菜单 · " + String(e), true, id));
  }
  const palette = ["#336C60", "#58669C", "#836534", "#586B3B", "#905D72", "#3C7085"];
  const tileColor = computed(() => palette[Array.from(props.app.package).reduce((n, c) => n + c.charCodeAt(0), 0) % palette.length]);
  return <View onContextMenu={e => menu(e.pagePosition)} style={{ width: 138, padding: 10, gap: 5, backgroundColor: colors().card,
      borderWidth: 1, borderColor: colors().line, borderRadius: 12 }}>
    <View style={{ flexDirection: "row", justifyContent: "space-between", alignItems: "flex-start" }}>
      <View testID={"app-" + props.app.package} role="button" accessibilityLabel={"打开 " + props.app.label} tooltip={props.app.label + " · " + props.app.package}
        disabled={!enabled()} onPress={() => { const action = launch(); if (action) runAction(props.instanceId, props.app, action); }}
        style={{ flex: 1, gap: 8, minWidth: 0, opacity: enabled() ? 1 : 0.64, borderWidth: 1, borderRadius: 5, borderColor: colors().card }}
        focusVisibleStyle={{ borderColor: colors().accent }}>
        <View style={{ width: 42, height: 42, borderRadius: 12, backgroundColor: tileColor(), justifyContent: "center", alignItems: "center" }}>
          <Text style={{ fontSize: 21, fontWeight: 650, color: "#FFFFFF" }}>{Array.from(props.app.label || props.app.package)[0]}</Text>
        </View>
        <Text style={{ fontSize: 13, fontWeight: 600, numberOfLines: 2, color: colors().ink }}>{props.app.label}</Text>
      </View>
      <View testID={"menu-" + props.app.package} role="button" accessibilityLabel={props.app.label + "的操作"}
        tooltip="应用操作" onPress={() => menu()} style={{ paddingHorizontal: 8, paddingVertical: 2, borderRadius: 7, borderWidth: 1, borderColor: colors().card }}
        hoverStyle={{ backgroundColor: colors().wash }} focusVisibleStyle={{ borderColor: colors().accent }}>
        <Text style={{ fontSize: 16, fontWeight: 600, color: colors().muted }}>···</Text>
      </View>
    </View>
    {props.app.package === snapshot()?.desktop?.defaultPackage ? <Text style={{ fontSize: 11, color: colors().accent }}>默认桌面</Text> : null}
  </View>;
}

const groups = computed(() => snapshot()?.groups ?? []);

const customGroups = computed(() => groups().filter(g => g.id.startsWith("group-")));

function GroupEditor(props: { selected: string; select: (id: string) => void }) {
  const owner = signal<string | null>(null);
  const name = signal("");
  const invalid = computed(() => {
    const value = name().trim();
    return !value || Array.from(value).length > 40 || /[\u0000-\u001f\u007f-\u009f]/.test(value);
  });
  const custom = computed(() => groups().find(g => g.id === props.selected && g.id.startsWith("group-")));
  async function save() {
    const id = owner(), value = name().trim();
    if (!id || id !== currentId() || invalid() || busy()) return;
    if (await send({ kind: "createGroup", instanceId: id, name: value }, "创建分组")) {
      owner.set(null);
      if (currentId() === id) props.select(groups().find(g => g.name === value)?.id ?? "");
    }
  }
  function remove() {
    const id = currentId(), g = custom();
    if (!id || !g) return;
    confirmation.set({ id, title: "删除「" + g.name + "」分组？", detail: "删除后仍可在「已安装」或「系统应用」打开组内应用。", label: "删除分组",
      action: async () => { if (await send({ kind: "deleteGroup", instanceId: id, groupId: g.id }, "删除分组") && id === currentId()) props.select(""); } });
  }
  return <View style={row}>
    {owner() !== null && currentId() === owner() ? <>
      <Input testID="group-name" accessibilityLabel="分组名称" placeholder="分组名称" value={name()} autoFocus onChange={e => name.set(e.value)} onSubmit={save}
        style={{ width: 160, paddingHorizontal: 8, minHeight: 32, borderRadius: 7, borderWidth: 1, borderColor: colors().line, color: colors().ink }} />
      <Button id="save-group" label="保存分组" primary disabled={busy() || invalid()} press={save} />
      <Button id="cancel-group" label="取消" press={() => owner.set(null)} />
    </> : <>
      <Button id="new-group" label="新建分组…" disabled={busy()} press={() => { name.set(""); owner.set(currentId()); }} />
      {custom() ? <Button id="delete-group" label="删除分组…" danger disabled={busy()} press={remove} /> : null}
    </>}
  </View>;
}

function Catalog(props: { showGroups: boolean }) {
  const groupId = signal("");
  const filter = computed(() => props.showGroups ? customGroups().find(g => g.id === groupId()) : undefined);
  let groupOwner = currentId();
  effect(() => {
    if (currentId() !== groupOwner) {
      groupOwner = currentId();
      groupId.set("");
    }
  });
  // 自建分组可能含有不同来源的包；所有视图均以包自身的 system 标志归类。
  const catalogApps = computed(() => (filter()?.apps ?? groups().flatMap(g => g.apps)).filter(app => app.launchable)
    .sort((a, b) => a.label.localeCompare(b.label)));
  const sections = computed(() => [
    { id: "installed", title: "已安装", apps: catalogApps().filter(app => !app.system) },
    { id: "system", title: "系统应用", apps: catalogApps().filter(app => app.system) },
  ]);
  const status = computed(() => snapshot()?.catalogStatus?.kind ?? "pending");
  return <View style={{ gap: 16 }}>
    {props.showGroups ? <View style={{ gap: 8 }}>
      <View style={row}>
        <Button id="group-all" label="全部分组" primary={!filter()} press={() => groupId.set("")} />
        <Each of={customGroups()} key={g => g.id}>{g =>
          <Button id={"group-" + g().id} label={g().name} primary={g().id === groupId()} press={() => groupId.set(g().id)} />
        }</Each>
      </View>
      <GroupEditor selected={groupId()} select={id => groupId.set(id)} />
    </View> : null}
    {status() === "failed" ? <Text testID="catalog-error" role="alert" style={{ fontSize: 13, color: colors().bad }}>
      {"应用列表读取失败 · " + (snapshot()?.catalogStatus?.message || "请刷新列表重试")}</Text> : null}
    {status() === "pending" ? <Detail id="catalog-pending" text={can("refreshApps") ? "正在读取应用…" : "实例就绪后将读取应用列表。"} /> : null}
    {filter() && !catalogApps().length && status() === "ready" ? <Detail text="这个分组还没有应用" /> : null}
    <Each of={sections()} key={section => section.id}>{section =>
      <If when={section().apps.length > 0 || (!filter() && status() === "ready")}>
        <View testID={"section-" + section().id} style={{ gap: 8 }}>
          <Heading text={section().title} />
          {section().apps.length ? <View style={{ flexDirection: "row", flexWrap: "wrap", gap: 10 }}>
            <Each of={section().apps} key={app => app.package}>{app => <AppTile app={app()} instanceId={currentId()!} />}</Each>
          </View> : section().id === "installed" ? <View testID="empty-library" style={{ gap: 4 }}>
            <Detail text={can("install") ? "拖入 APK，或点「安装 APK…」选择文件。" : "尚无已安装的应用。"} />
          </View> : <Detail text="没有可打开的系统应用。" />}
        </View>
      </If>
    }</Each>
  </View>;
}

export function Library(props: { onManage: () => void }) {
  const tab = signal<"apps" | "tasks">("apps");
  const showGroups = signal(false);
  let owner = currentId();
  effect(() => {
    if (currentId() !== owner) {
      owner = currentId();
      tab.set("apps");
      showGroups.set(false);
    }
  });
  const taskLabel = computed(() => tasksKnown() ? "运行中 · " + running().length : "运行中");
  const tabStyle = { minHeight: 32, paddingHorizontal: 12, justifyContent: "center", borderWidth: 1, borderRadius: 7 } as const;
  return <View testID="library" style={{ gap: 14 }}>
    <View style={{ ...row, justifyContent: "space-between" }}>
      <Title text={selected()?.name ?? "应用库"} />
      <View style={row}>
        <Button id="open-device" label="设备视图" disabled={!can("deviceView") || busy()} press={() => act("openDevice", "打开设备视图")} />
        <Button id="open-management" label="管理实例" disabled={switching()} press={props.onManage} />
      </View>
    </View>
    <View style={{ ...row, justifyContent: "space-between" }}>
      <DeviceStatus />
      <LifecycleButtons />
    </View>
    <View style={card()}>
      <View style={{ ...row, justifyContent: "space-between" }}>
        <Tabs style={{ flexDirection: "row", gap: 2 }}>
          <Tab testID="panel-apps" selected={tab() === "apps"} onPress={() => tab.set("apps")}
            style={{ ...tabStyle, borderColor: colors().card }} selectedStyle={{ backgroundColor: colors().selected }}
            focusVisibleStyle={{ borderColor: colors().accent }}>
            <Text style={{ fontSize: 14, fontWeight: 600, color: tab() === "apps" ? colors().accent : colors().muted }}>应用</Text>
          </Tab>
          <Tab testID="panel-tasks" selected={tab() === "tasks"} onPress={() => tab.set("tasks")}
            style={{ ...tabStyle, borderColor: colors().card }} selectedStyle={{ backgroundColor: colors().selected }}
            focusVisibleStyle={{ borderColor: colors().accent }}>
            <Text style={{ fontSize: 14, fontWeight: 600, color: tab() === "tasks" ? colors().accent : colors().muted }}>{taskLabel()}</Text>
          </Tab>
        </Tabs>
        <View style={{ ...row, visibility: tab() === "apps" ? "visible" : "hidden" }}>
          <Button id="show-groups" label={showGroups() ? "收起分组" : "分组"} press={() => showGroups.set(!showGroups())} />
          <Button id="refresh-apps" label="刷新列表" disabled={!can("refreshApps") || busy()} press={() => act("refreshApps", "刷新应用")} />
          <Button id="add-apk" label="安装 APK…" primary disabled={!can("install") || busy()} press={() => act("addApk", "安装 APK")} />
        </View>
      </View>
      <Scroll testID={tab() === "apps" ? "apk-drop" : "task-scroll"}
        onFileDrop={tab() === "apps" ? e => dropApk(e.uris) : undefined} accessibilityLabel={tab() === "apps" ? "应用列表" : "运行中的应用"}
        style={{ height: Math.max(220, windowSize().height - 260), minWidth: 0, borderTopWidth: 1, borderColor: colors().line, paddingTop: 12 }}>
        {tab() === "apps" ? <Catalog showGroups={showGroups()} /> : <RunningList />}
      </Scroll>
    </View>
  </View>;
}
