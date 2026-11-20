import { cleanup, host, Scroll, signal, Text, View } from "nsh";
import { Menu } from "nsh/desktop";
import { Button, card, colors, Detail, Heading, row, Title } from "./controls";
import { openGuide, settingsTab } from "./model";
import { Storage } from "./storage";

const choices = { ask: "每次询问", background: "保留后台", quit: "完全退出" };
type CloseAction = keyof typeof choices;
interface WindowSettings { closeAction: CloseAction }

const tabs = [
  { id: "window", name: "应用窗口" },
  { id: "storage", name: "存储位置" },
  { id: "logs", name: "应用日志" },
] as const;


function WindowSettingsPanel() {
  const action = signal<CloseAction | null>(null);
  const error = signal("");
  const saving = signal(false);
  let revision = 0;
  const show = (settings: WindowSettings) => {
    action.set(settings.closeAction);
    error.set("");
  };
  cleanup(host.on<WindowSettings>("windroid.windowSettingsChanged", data => { ++revision; show(data); }));
  async function read() {
    const request = ++revision;
    try {
      const settings = await host.call<WindowSettings>("windroid.windowSettings");
      if (request === revision) show(settings);
    } catch (e) { if (request === revision) error.set("读取窗口设置失败 · " + String(e)); }
  }
  async function save(closeAction: CloseAction) {
    saving.set(true);
    try {
      show(await host.call<WindowSettings>("windroid.setWindowSettings", { closeAction }));
    } catch (e) { error.set("保存窗口设置失败 · " + String(e)); }
    finally { saving.set(false); }
  }
  async function choose() {
    try {
      await Menu.popup((Object.keys(choices) as CloseAction[]).map(k => ({
        label: choices[k], checked: k === action(), onSelect: () => void save(k),
      })));
    } catch (e) { error.set("打开窗口设置失败 · " + String(e)); }
  }
  void read();
  return <View style={card()}>
      <Heading text="应用窗口" />
      <Detail text="关闭仍在运行的应用窗口时，所有实例共用此选择。" />
      <Button id="close-preference" label={action() ? choices[action()!] + "  ▾" : "读取关闭偏好…"}
        disabled={action() === null || saving()} press={choose} />
      {error() ? <View style={{ gap: 6 }}><Text role="alert" style={{ color: colors().bad }}>{error()}</Text>
        <Button id="retry-settings" label="重新读取" press={read} /></View> : null}
  </View>;
}

export function Settings() {
  return <View testID="settings" style={{ flex: 1, minHeight: 0, gap: 16 }}>
    <View style={{ ...row, justifyContent: "space-between" }}>
      <Title text="设置" /><Button id="show-guide" label="引导" press={() => openGuide("settings")} />
    </View>
    <View role="tablist" accessibilityLabel="设置分类" style={row}>
      <Each of={tabs} key={tab => tab.id}>{tab =>
        <View testID={"settings-" + tab().id} role="tab" accessibilityLabel={tab().name}
          accessibilityState={{ selected: settingsTab() === tab().id }} onPress={() => settingsTab.set(tab().id)}
          style={{ paddingVertical: 7, paddingHorizontal: 12, borderRadius: 6,
            backgroundColor: settingsTab() === tab().id ? colors().selected : colors().canvas }}
          focusVisibleStyle={{ backgroundColor: colors().wash }}>
          <Text style={{ fontWeight: 600, fontSize: 13, color: colors().ink }}>{tab().name}</Text>
        </View>
      }</Each>
    </View>
    <Scroll testID="workspace-scroll" style={{ flex: 1, minHeight: 0 }}>
    {settingsTab() === "window" ? <WindowSettingsPanel /> : settingsTab() === "storage" ? <Storage /> : <View style={card()}>
      <Heading text="应用日志" /><Detail text="在终端直接运行 Windroid 可执行文件，可以看到写入标准错误的应用日志。" />
    </View>}
    </Scroll>
  </View>;
}
