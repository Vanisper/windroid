import { computed, mount, signal, Text, View } from "nsh";
import { host } from "nsh";

/**
 * 设备状态。字段名与 Rust 侧 contract 的 serde 形态一致。
 */
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
}

interface Snapshot {
  id: string;
  state: DeviceState;
  phase: "waiting-adb" | "warming" | "starting" | null;
  facts: Facts;
  apps: { package: string; system: boolean; label: string | null }[];
  seq: number;
  /** 宿主给出的一句话说明。文案只在 Rust 侧维护，此处不自行拼接。 */
  summary: string;
  /** 卡在哪一步：process / adb / package / activity / window。 */
  stuckAt: "process" | "adb" | "package" | "activity" | "window" | null;
}

interface Ack {
  status: "accepted" | "rejected";
  seq?: number;
  reason?: { kind: string; current?: DeviceState; wanted?: string; detail?: string };
}

/**
 * 状态名。只用于「当前 X，需要 Y」这类拒绝说明——说明性文案由宿主给出
 * （见 Snapshot.summary），这里只提供状态本身的名字。
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

/**
 * 卡点的人话说法。宿主给出标识，文案在此——因为它要接在「卡在」后面，
 * 属于界面措辞而非领域语义。
 */
const STUCK_TEXT: Record<string, string> = {
  process: "模拟器进程",
  adb: "adb 识别",
  package: "package 服务",
  activity: "activity 服务",
  window: "window 服务",
};

/** 服务齐备的数目。门限是三个都齐，不是 pm 可用。 */
function serviceCount(s: Services): number {
  return Number(s.activity) + Number(s.package) + Number(s.window);
}

// ---------------------------------------------------------------------------
// 界面状态：放在模块级。
//
// 组件函数体每次重算都会重新执行，若把 signal 与订阅写在那里，每次渲染都会
// 新建一份状态并重复订阅——表现为「数据拿到了但界面不更新」。状态属于应用，
// 不属于某一次渲染。
// ---------------------------------------------------------------------------

const snap = signal<Snapshot | null>(null);
const notice = signal("");
const busy = signal(false);

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

// 订阅一次，只订阅一次。事件到达后重取快照而不是直接拼事件载荷：
// 界面只读一个数据源，才不会出现半更新的中间态。
host.on("windroid.event", () => refresh());
refresh();

function App() {
  // 派生值必须用 computed：在组件体内直接读信号只在首次渲染取一次值，
  // 后续更新不会反映到界面。这是响应式最容易踩的一处。
  const state = computed<DeviceState>(() => snap()?.state ?? "absent");
  const summary = computed<string>(() => snap()?.summary ?? "正在连接设备域");
  // 卡点只在启动过程里展示。用户的应对随卡点完全不同——这是 Stalled 的核心信息。
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

  const canCreate = computed<boolean>(() => state() === "absent");
  const canStart = computed<boolean>(
    () => state() === "stopped" || state() === "failed",
  );
  const canStop = computed<boolean>(
    () => state() === "ready" || state() === "streaming" || state() === "stalled",
  );

  return (
    <View style={{ flexDirection: "column", height: "100%", backgroundColor: "#0e0e11" }}>
      <View style={{ padding: 20, flexDirection: "column", gap: 4 }}>
        <Text style={{ fontSize: 18, color: "#f0f0f4" }}>Windroid</Text>
        <Text testID="summary" style={{ fontSize: 12, color: "#9a9aa8" }}>
          {summary()}
        </Text>
      </View>

      <View style={{ paddingHorizontal: 20, flexDirection: "column", gap: 6 }}>
        {/* 三项服务固定，直接写出来比 map 更清楚——也避免 key 属性：
            nsh 的行复用（列表虚拟化）走 nsh/list 的 Each，普通 map 不需要 key。 */}
        <ServiceLed name="activity" on={activity()} />
        <ServiceLed name="package" on={pkg()} />
        <ServiceLed name="window" on={win()} />
        <Text testID="services" style={{ fontSize: 12, color: "#5a5a68" }}>
          {servicesLine()}
        </Text>
        {stuck() ? (
          <Text testID="stuck" style={{ fontSize: 12, color: "#c8a06a" }}>
            {stuck()}
          </Text>
        ) : null}
      </View>

      <View style={{ padding: 20, flexDirection: "row", gap: 10 }}>
        <Button
          label="创建实例"
          testID="create"
          disabled={!canCreate() || busy()}
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
        <Button
          label="启动"
          testID="start"
          disabled={!canStart() || busy()}
          onPress={() => send({ kind: "start" }, "启动")}
        />
        <Button
          label="停止"
          testID="stop"
          disabled={!canStop() || busy()}
          onPress={() => send({ kind: "stop" }, "停止")}
        />
      </View>

      {notice() ? (
        <View style={{ paddingHorizontal: 20 }}>
          <Text testID="notice" style={{ fontSize: 12, color: "#c8a06a" }}>
            {notice()}
          </Text>
        </View>
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
      <Text style={{ fontSize: 12, color: "#9a9aa8" }}>{props.name}</Text>
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
        paddingVertical: 7,
        paddingHorizontal: 14,
        borderRadius: 6,
        backgroundColor: props.disabled ? "#1b1b21" : "#2a2a34",
      }}
    >
      <Text style={{ fontSize: 13, color: props.disabled ? "#4a4a56" : "#e4e4ea" }}>
        {props.label}
      </Text>
    </View>
  );
}

mount(App);
