import { computed, mount, nativeComponent, signal, Text, View } from "nsh";
import { host } from "nsh";

/**
 * 视频表面。由 Rust 侧注册的同名原生组件承载。
 *
 * 这里只声明名字与占位，不涉及解码或纹理——那些在 Rust 侧。
 * 界面这一层的职责是「在布局里给它一个位置」。
 */
const VideoSurface = nativeComponent("windroid.video", { props: [] });

// ---------------------------------------------------------------------------
// 与宿主之间的数据形态
//
// 字段名与 Rust 侧 serde 的 rename_all = "camelCase" 一致。类型检查会
// 发现拼写错误，但改不了 Rust 字段名——两侧改名必须一起改。
// ---------------------------------------------------------------------------

type DeviceState =
  | "absent"
  | "stopped"
  | "starting"
  | "stalled"
  | "ready"
  | "streaming"
  | "stopping"
  | "failed";

interface Services {
  activity: boolean;
  package: boolean;
  window: boolean;
}

interface Facts {
  dirExists: boolean;
  processAlive: boolean;
  everLaunched: boolean;
  adbVisible: boolean;
  services: Services;
  bootCompleted: boolean;
  sessionCount: number;
  stopRequested: boolean;
  bootStalled: boolean;
  aborted: boolean;
}

/** 当前状态下允许的操作。由宿主按状态推导，界面只读。 */
interface Capabilities {
  create: boolean;
  delete: boolean;
  start: boolean;
  stop: boolean;
  launchApp: boolean;
  install: boolean;
  refreshApps: boolean;
  deviceView: boolean;
  transitioning: boolean;
}

interface ComponentStatus {
  name: string;
  ok: boolean;
  path: string | null;
  remedy: string;
}

interface Readiness {
  usable: boolean;
  avds: string[];
  components: ComponentStatus[];
}

/** 一条可执行的操作。由宿主派生，界面只渲染，不判断。 */
interface Action {
  kind: "launch" | "setDefaultHome" | "forceStop" | "clearData";
  label: string;
  enabled: boolean;
  reason: string | null;
  destructive: boolean;
}

interface AppItem {
  package: string;
  label: string;
  system: boolean;
  launchable: boolean;
  actions: Action[];
}

interface Group {
  id: string;
  name: string;
  count: number;
  apps: AppItem[];
}

interface Snapshot {
  id: string;
  state: DeviceState;
  phase: "waiting-adb" | "warming" | "starting" | null;
  facts: Facts;
  apps: { package: string; system: boolean; label: string | null }[];
  seq: number;
  /** 宿主给出的一句话说明。文案只在 Rust 侧维护。 */
  summary: string;
  /** 卡在哪一步：process / adb / package / activity / window。 */
  stuckAt: "process" | "adb" | "package" | "activity" | "window" | null;
  capabilities: Capabilities;
  readiness: Readiness | null;
  groups: Group[];
  /** 当前默认桌面的包名。空表示没有。 */
  desktop: { defaultPackage: string | null } | null;
}

interface Ack {
  status: "accepted" | "rejected";
  seq?: number;
  reason?: { kind: string; current?: DeviceState; wanted?: string; detail?: string };
}

/**
 * 状态名。用于「当前 X，需要 Y」这类拒绝说明与侧边栏状态灯。
 * 说明性文案由宿主给出（Snapshot.summary），这里只提供状态本身的名字。
 */
const STATE_NAME: Record<DeviceState, string> = {
  absent: "尚未创建",
  stopped: "已停止",
  starting: "正在启动",
  stalled: "启动较慢",
  ready: "就绪",
  streaming: "运行中",
  stopping: "正在停止",
  failed: "启动失败",
};

/** 卡点的人话说法。接在「卡在」之后，属于界面措辞。 */
const STUCK_TEXT: Record<string, string> = {
  process: "模拟器进程",
  adb: "adb 识别",
  package: "package 服务",
  activity: "activity 服务",
  window: "window 服务",
};

/** 服务齐备的数目。门限是三个都齐。 */
function serviceCount(s: Services): number {
  return Number(s.activity) + Number(s.package) + Number(s.window);
}

/**
 * 状态灯的颜色。
 *
 * 过渡态用琥珀色而不是继续用灰：用户需要知道「它在动」，这在启动那几十秒里
 * 是唯一有用的信息。
 */
function stateColor(s: DeviceState): string {
  switch (s) {
    case "ready":
    case "streaming":
      return "#4ade80";
    case "starting":
    case "stopping":
      return "#e0b34a";
    case "stalled":
      return "#c8a06a";
    case "failed":
      return "#ef5f5f";
    default:
      return "#3f3f4a";
  }
}

// ---------------------------------------------------------------------------
// 应用状态：放在模块级。
//
// 组件函数体每次重算都会重新执行。把 signal 与订阅写在里面，每次渲染都会
// 新建一份状态并重复订阅——表现为「数据拿到了但界面不更新」。状态属于应用，
// 不属于某一次渲染。
// ---------------------------------------------------------------------------

const snap = signal<Snapshot | null>(null);
const notice = signal("");
const busy = signal(false);
/** 当前选中的分组 id。空表示用第一个分组。 */
const groupId = signal("");

/** 重新取一次快照。事件到达后与启动时都走这里。 */
function refresh() {
  host.call<Snapshot>("windroid.snapshot").then(
    (v) => snap.set(v),
    (e: Error & { code?: string }) =>
      notice.set(`取快照失败: ${e.code ?? ""} ${e.message}`),
  );
}

/** 发一条命令。回执只表示受理，完成通过事件告知。 */
function send(cmd: unknown, label: string) {
  busy.set(true);
  host.call<Ack>("windroid.command", cmd).then(
    (ack) => {
      busy.set(false);
      if (ack.status === "rejected") {
        const r = ack.reason;
        // 预期内的状况与真正的错误分开说：前者用户能自己处理。
        notice.set(
          r?.kind === "wrongState"
            ? `${label}：当前${STATE_NAME[r.current ?? "absent"]}，需要${r.wanted ?? "其他状态"}`
            : `${label}失败: ${r?.detail ?? r?.kind ?? "未知原因"}`,
        );
      } else {
        notice.set(`${label}已受理`);
      }
      refresh();
    },
    (e: Error) => {
      busy.set(false);
      notice.set(`${label}出错: ${e.message}`);
    },
  );
}

/** 执行一个应用操作。形状与宿主约好：kind + package。 */
function runAction(app: AppItem, kind: Action["kind"]) {
  const action = app.actions.find((x) => x.kind === kind);
  if (!action || !action.enabled) {
    notice.set(action?.reason ?? "此操作当前不可用");
    return;
  }
  send({ kind, package: app.package }, `${action.label} ${app.label}`);
}

// 订阅一次。事件到达后重取快照而不是拼事件载荷：界面只读一个数据源，
// 才不会出现半更新的中间态。
host.on("windroid.event", () => refresh());
refresh();

// ---------------------------------------------------------------------------
// 界面
// ---------------------------------------------------------------------------

function App() {
  // 派生值必须用 computed：在组件体内直接读信号只取一次值，后续更新不会
  // 反映到界面。这是响应式最容易踩的一处。
  const state = computed<DeviceState>(() => snap()?.state ?? "absent");
  const summary = computed<string>(() => snap()?.summary ?? "正在连接设备域");
  const caps = computed<Capabilities | null>(() => snap()?.capabilities ?? null);
  const readiness = computed<Readiness | null>(() => snap()?.readiness ?? null);

  const stuck = computed<string | null>(() => {
    const s = snap();
    if (!s || (s.state !== "starting" && s.state !== "stalled")) return null;
    return s.stuckAt ? `卡在${STUCK_TEXT[s.stuckAt] ?? s.stuckAt}` : null;
  });

  const servicesLine = computed<string>(() => {
    const s = snap();
    const n = s ? serviceCount(s.facts.services) : 0;
    return `服务 ${n}/3 · adb ${s?.facts.adbVisible ? "已连接" : "未连接"}`;
  });

  const activity = computed<boolean>(() => snap()?.facts.services.activity ?? false);
  const pkg = computed<boolean>(() => snap()?.facts.services.package ?? false);
  const win = computed<boolean>(() => snap()?.facts.services.window ?? false);

  // 按钮可用性一律取自宿主推导的 caps，不在界面里另写一套判断。
  const can = (k: keyof Capabilities): boolean => caps()?.[k] === true;

  // 组件缺失时优先展示自检结果：用户此刻需要的是「缺什么、怎么补」，
  // 而不是「启动失败」。
  const blocked = computed<ComponentStatus[]>(() => {
    const r = readiness();
    return r ? r.components.filter((c) => !c.ok) : [];
  });

  return (
    <View style={{ flexDirection: "row", height: "100%", backgroundColor: "#0e0e11" }}>
      <Sidebar
        state={state()}
        summary={summary()}
        avds={readiness()?.avds ?? []}
        stuck={stuck()}
        servicesLine={servicesLine()}
        activity={activity()}
        pkg={pkg()}
        win={win()}
        canCreate={can("create")}
        canStart={can("start")}
        canStop={can("stop")}
        busy={busy()}
      />

      <View style={{ flex: 1, flexDirection: "column" }}>
        {blocked().length > 0 ? (
          <BlockedPanel items={blocked()} />
        ) : (
          <Library
            groups={snap()?.groups ?? []}
            selected={groupId()}
            canLaunch={can("launchApp")}
            canInstall={can("install")}
            state={state()}
            defaultPackage={snap()?.desktop?.defaultPackage ?? null}
          />
        )}

        {notice() ? (
          <View style={{ paddingHorizontal: 16, paddingVertical: 8 }}>
            <Text testID="notice" style={{ fontSize: 12, color: "#c8a06a" }}>
              {notice()}
            </Text>
          </View>
        ) : null}
      </View>
    </View>
  );
}

/**
 * 侧边栏：实例与分组。
 *
 * 实例放在这里而不是下拉框：多实例是一等公民，它该一直看得见——哪个在运行、
 * 哪个停了、哪个是当前。
 */
function Sidebar(props: {
  state: DeviceState;
  summary: string;
  avds: string[];
  stuck: string | null;
  servicesLine: string;
  activity: boolean;
  pkg: boolean;
  win: boolean;
  canCreate: boolean;
  canStart: boolean;
  canStop: boolean;
  busy: boolean;
}) {
  return (
    <View
      style={{
        width: 220,
        flexDirection: "column",
        paddingVertical: 12,
        paddingHorizontal: 12,
        gap: 10,
        backgroundColor: "#121216",
      }}
    >
      <Text style={{ fontSize: 15, color: "#f0f0f4" }}>Windroid</Text>

      <Text style={{ fontSize: 11, color: "#5a5a68" }}>实例</Text>

      <InstanceRow
        name={props.avds[0] ?? "尚未创建"}
        state={props.state}
        summary={props.summary}
        stuck={props.stuck}
      />

      <View style={{ flexDirection: "row", gap: 8 }}>
        <Button
          label="创建实例"
          testID="create"
          disabled={!props.canCreate || props.busy}
          onPress={() =>
            send(
              {
                kind: "create",
                spec: {
                  template: "phone-1080",
                  image: { api: 34, tag: "google_apis_playstore", abi: "arm64-v8a" },
                  diskBytes: 8 * 1024 * 1024 * 1024,
                },
              },
              "创建",
            )
          }
        />
      </View>

      <View style={{ flexDirection: "row", gap: 8 }}>
        <Button
          label="启动"
          testID="start"
          disabled={!props.canStart || props.busy}
          onPress={() => send({ kind: "start" }, "启动")}
        />
        <Button
          label="停止"
          testID="stop"
          disabled={!props.canStop || props.busy}
          onPress={() => send({ kind: "stop" }, "停止")}
        />
      </View>

      <View style={{ flexDirection: "column", gap: 5 }}>
        <ServiceLed name="activity" on={props.activity} />
        <ServiceLed name="package" on={props.pkg} />
        <ServiceLed name="window" on={props.win} />
        <Text testID="services" style={{ fontSize: 11, color: "#5a5a68" }}>
          {props.servicesLine}
        </Text>
      </View>
    </View>
  );
}

/** 一行实例：状态灯 + 名字 + 一句话说明。 */
function InstanceRow(props: {
  name: string;
  state: DeviceState;
  summary: string;
  stuck: string | null;
}) {
  return (
    <View
      style={{
        flexDirection: "row",
        gap: 8,
        alignItems: "flex-start",
        paddingVertical: 6,
        paddingHorizontal: 8,
        borderRadius: 6,
        backgroundColor: "#1b1b21",
      }}
    >
      <View
        style={{
          width: 7,
          height: 7,
          borderRadius: 4,
          marginTop: 5,
          backgroundColor: stateColor(props.state),
        }}
      />
      <View style={{ flex: 1, flexDirection: "column", gap: 2 }}>
        <Text style={{ fontSize: 12, color: "#e4e4ea" }}>{props.name}</Text>
        <Text testID="summary" style={{ fontSize: 11, color: "#8a8a98" }}>
          {props.summary}
        </Text>
        {props.stuck ? (
          <Text testID="stuck" style={{ fontSize: 11, color: "#c8a06a" }}>
            {props.stuck}
          </Text>
        ) : null}
      </View>
    </View>
  );
}

/**
 * 运行时缺失时的提示。
 *
 * 用户此刻需要的是「缺什么、后果、怎么补」，不是错误码。每一项都给出
 * 可照做的说明。
 */
function BlockedPanel(props: { items: ComponentStatus[] }) {
  return (
    <View style={{ flex: 1, padding: 20, flexDirection: "column", gap: 10 }}>
      <Text style={{ fontSize: 14, color: "#e4e4ea" }}>运行环境未就绪</Text>
      {props.items.map((c) => (
        <View style={{ flexDirection: "column", gap: 2 }}>
          <Text style={{ fontSize: 12, color: "#ef5f5f" }}>{c.name}</Text>
          <Text style={{ fontSize: 11, color: "#8a8a98" }}>{c.remedy}</Text>
        </View>
      ))}
    </View>
  );
}

/**
 * 应用库：分组标签 + 图标网格。
 *
 * 分组与归属由宿主给出，界面只渲染。界面不判断「这个应用算不算系统应用」——
 * 那套判定依赖 LAUNCHER/HOME 两个 intent 与 pm 的分类，复制一遍必然漂移。
 */
function Library(props: {
  groups: Group[];
  selected: string;
  canLaunch: boolean;
  canInstall: boolean;
  state: DeviceState;
  defaultPackage: string | null;
}) {
  if (props.groups.length === 0) {
    const hint =
      props.state === "ready" || props.state === "streaming"
        ? "此实例没有可显示的应用。"
        : "启动实例后在此查看应用。";
    return (
      <View
        style={{
          flex: 1,
          alignItems: "center",
          justifyContent: "center",
        }}
      >
        <Text style={{ fontSize: 13, color: "#5a5a68" }}>{hint}</Text>
      </View>
    );
  }

  const current =
    props.groups.find((g) => g.id === props.selected) ?? props.groups[0];

  return (
    <View style={{ flex: 1, flexDirection: "column" }}>
      {/* 分组标签。选中态用实心胶囊，未选用描边——同色系下靠填充区分，
          不靠颜色（颜色已被状态灯占用）。 */}
      <View
        style={{
          flexDirection: "row",
          gap: 8,
          paddingHorizontal: 16,
          paddingVertical: 12,
          alignItems: "center",
        }}
      >
        {props.groups.map((g) => {
          const on = g.id === current.id;
          return (
            <View
              onPress={() => groupId.set(g.id)}
              style={{
                flexDirection: "row",
                gap: 6,
                alignItems: "center",
                paddingHorizontal: 10,
                paddingVertical: 4,
                borderRadius: 999,
                backgroundColor: on ? "#3a5fcd" : "#1b1b21",
              }}
            >
              <Text style={{ fontSize: 12, color: on ? "#ffffff" : "#9a9aa8" }}>
                {g.name}
              </Text>
              <Text style={{ fontSize: 10, color: on ? "#c8d4f8" : "#5a5a68" }}>
                {String(g.count)}
              </Text>
            </View>
          );
        })}
      </View>

      <View
        style={{
          flexDirection: "row",
          flexWrap: "wrap",
          gap: 14,
          paddingHorizontal: 16,
          paddingBottom: 16,
        }}
      >
        {current.apps.map((app) => (
          <AppCard
            app={app}
            defaultHome={app.package === (props.defaultPackage ?? "")}
            canLaunch={props.canLaunch}
            onPress={() => runAction(app, "launch")}
          />
        ))}
      </View>
    </View>
  );
}

/** 图标下的标签最多 8 个字符，超出用省略号。 */
function shortLabel(label: string): string {
  return label.length <= 8 ? label : label.slice(0, 8) + "…";
}

/**
 * 一个应用图标。
 *
 * 显示哪些信息由数据决定（是否默认桌面），能做什么由宿主派生的 actions
 * 决定——界面不判断「这个应用能不能当桌面」。不能启动的应用照样显示，
 * 只是点不动：隐藏它等于抹掉事实。
 */
function AppCard(props: {
  app: AppItem;
  defaultHome: boolean;
  canLaunch: boolean;
  onPress: () => void;
}) {
  const launch = props.app.actions.find((x) => x.kind === "launch");
  const enabled = props.canLaunch && (launch?.enabled ?? false);
  return (
    <View
      testID={`app-${props.app.package}`}
      accessibilityLabel={props.app.label}
      onPress={enabled ? props.onPress : undefined}
      style={{ width: 92, flexDirection: "column", gap: 5, alignItems: "center" }}
    >
      <View
        style={{
          width: 56,
          height: 56,
          borderRadius: 14,
          backgroundColor: enabled ? "#2a2a34" : "#1b1b21",
          alignItems: "center",
          justifyContent: "center",
        }}
      >
        <Text style={{ fontSize: 18, color: enabled ? "#e4e4ea" : "#4a4a56" }}>
          {props.app.label.slice(0, 1)}
        </Text>
      </View>
      <Text style={{ fontSize: 11, color: enabled ? "#c8c8d4" : "#5a5a68" }}>
        {shortLabel(props.app.label)}
      </Text>
      {props.defaultHome ? (
        <Text style={{ fontSize: 9, color: "#4ade80" }}>默认桌面</Text>
      ) : null}
    </View>
  );
}

/** 一项系统服务的状态指示。绿色表示已在运行。 */
function ServiceLed(props: { name: string; on: boolean }) {
  return (
    <View style={{ flexDirection: "row", gap: 8, alignItems: "center" }}>
      <View
        style={{
          width: 6,
          height: 6,
          borderRadius: 3,
          backgroundColor: props.on ? "#4ade80" : "#3f3f4a",
        }}
      />
      <Text style={{ fontSize: 11, color: "#9a9aa8" }}>{props.name}</Text>
    </View>
  );
}

function Button(props: {
  label: string;
  testID: string;
  disabled: boolean;
  onPress: () => void;
}) {
  return (
    <View
      testID={props.testID}
      accessibilityLabel={props.label}
      onPress={props.disabled ? undefined : props.onPress}
      style={{
        paddingVertical: 6,
        paddingHorizontal: 12,
        borderRadius: 6,
        backgroundColor: props.disabled ? "#1b1b21" : "#2a2a34",
      }}
    >
      <Text style={{ fontSize: 12, color: props.disabled ? "#4a4a56" : "#e4e4ea" }}>
        {props.label}
      </Text>
    </View>
  );
}

mount(App);
