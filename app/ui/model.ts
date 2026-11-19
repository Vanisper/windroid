import { computed, host, signal } from "nsh";

export type DeviceState = "absent" | "stopped" | "starting" | "stalled" | "ready" | "streaming" | "stopping" | "failed";
export const stateName: Record<DeviceState, string> = {
  absent: "尚未创建", stopped: "已停止", starting: "正在启动", stalled: "启动较慢",
  ready: "就绪", streaming: "运行中", stopping: "正在停止", failed: "启动失败",
};

export interface ImageIdentity { api: number; tag: string; abi: string; revision: number }

export function describeImage(image: ImageIdentity) {
  const variant = image.tag === "google_apis_playstore" ? "Google Play" : image.tag;
  return "API " + image.api + " · " + variant + " · " + image.abi + " · " + (image.revision > 0 ? "修订版 " + image.revision : "缺少修订号");
}

export interface InstanceConfig {
  template: string; image: ImageIdentity;
  cpuCores: number | null; ramMiB: number | null; screenWidth: number | null;
  screenHeight: number | null; diskBytes: number;
}
export interface Instance {
  id: string; name: string; state: DeviceState; frozen?: boolean; summary?: string;
  config?: InstanceConfig | null; observed?: { cpuCores: number | null; ramMiB: number | null } | null;
}
export const instanceState = (item: Instance) => item.frozen ? "已冻结" : stateName[item.state];
export interface Action {
  kind: "launch" | "setDefaultHome" | "forceStop" | "clearData";
  label: string; enabled: boolean; reason: string | null; destructive: boolean;
}
export interface AppItem { package: string; label: string; system: boolean; launchable: boolean; actions: Action[] }
interface AppSection { id: "installed" | "system"; name: string; count: number; apps: AppItem[] }
export interface RunningApp { instanceId: string; package: string; label: string; windowOpen: boolean }
export type Integrity = "missing" | "quick" | "verified" | "damaged" | "conflict";
export interface Component {
  id: string; name: string; version: string; source: string; sizeBytes: number; ready: boolean;
  license: string; licenseUrl: string; licenseText?: string;
  location: string; downloadedBytes: number; integrity: Integrity; issue: string | null;
  writable: boolean; requiredBytes: number; availableBytes: number | null;
  image: InstanceConfig["image"] | null; minEmulator: string | null;
}
export interface Preparation {
  phase: "missing" | "downloading" | "verifying" | "installing" | "paused" | "failed" | "ready";
  candidateTarget: string | null; candidateDownloadedBytes: number | null;
  componentsReady: boolean; components: Component[]; images: Component[]; downloadedBytes: number; totalBytes: number;
  currentComponent: string | null; error: string | null;
}
interface InstallError { code: string; detail: string }
export interface Message { instanceId: string | null; message: string; failed: boolean; seq?: number; installError?: InstallError }

// 错误分类来自宿主的 ErrorCode，拖入和原生文件面板共用一份面向用户的解释。
const installReasons: Readonly<Record<string, string | undefined>> = {
  "signature-mismatch": "APK 签名与已安装的应用不一致。",
  "version-downgrade": "APK 版本比已安装的版本低。",
  "out-of-space": "实例存储空间不足。",
  "invalid-apk": "APK 文件无效，请选择完整且已签名的安装包。",
  "component-missing": "安装 APK 所需的运行时组件缺失。",
  "io": "无法读取本机的 APK 文件。",
};
export function messageText(note: Message): string {
  const error = note.installError;
  if (!error) return note.message;
  const reason = installReasons[error.code];
  if (reason) return reason;
  if (error.detail.includes("INSTALL_FAILED_NO_MATCHING_ABIS")) return "APK 不支持此实例的处理器架构。";
  if (error.detail.includes("INSTALL_FAILED_")) return "Android 拒绝安装这个 APK。";
  return "无法确认 APK 安装成功。";
}
type Capability = "start" | "stop" | "delete" | "launchApp" | "install" | "deviceView" | "refreshApps";
export interface Snapshot {
  id: string; state: DeviceState; summary: string; stuckAt: string | null;
  capabilities: Record<Capability, boolean>; facts: { adbVisible: boolean; services: { activity: boolean; package: boolean; window: boolean } };
  catalogStatus?: { kind: "pending" | "ready" | "failed"; message?: string };
  groups: AppSection[]; frozen?: boolean; desktop: { defaultPackage: string | null; candidates?: string[]; candidateLabels?: Record<string, string> } | null; streamNote: string | null;
  // 单设备 Bridge 快照没有实例列表。显式的 selectedId:null 始终表示未选中。
  instances?: Instance[]; selectedId?: string | null; preparation?: Preparation; recommendedTemplate?: string; templateRamMiB?: Record<string, number>;
  showWelcome?: boolean;
  storage?: { home: string; runtimeRoot: string; imageRoot: string; defaultRuntimeRoot: string; defaultImageRoot: string; runtimeIsDefault: boolean; imageIsDefault: boolean; cleanupNotes: string[]; error?: string | null };
  running?: RunningApp[]; tasksLoaded?: boolean; runningError?: string | null; operation?: Message | null;
}

type DeviceCommand = "selectInstance" | "deleteInstance" | "start" | "stop" | "refreshApps" | "openDevice" | "addApk";
type AppCommand = Action["kind"] | "focusApp" | "backgroundApp" | "quitApp";
type Command =
  | { kind: "createInstance"; name: string; template: string; imageId: string; image: ImageIdentity }
  | { kind: "applyStorage"; plan: { previewId: number } }
  | { kind: "prepareCandidateImage"; previewId: number; resourceId: string; acceptedLicenses: true }
  | { kind: "prepareImage"; imageId: string; acceptedLicenses: true }
  | { kind: "importImage"; path: string; imageId: string; acceptedLicenses: true }
  | { kind: "repairResource"; resourceId: string; location: string; acceptedLicenses: true; confirmed: true }
  | { kind: "verifyResource"; resourceId: string }
  | { kind: "prepareRuntime"; acceptedLicenses: true }
  | { kind: "pausePreparation" | "recheckRuntime" }
  | { kind: DeviceCommand; instanceId: string }
  | { kind: "install"; instanceId: string; apkPath: string }
  | { kind: AppCommand; instanceId: string; package: string };
interface Ack { status: "accepted" | "rejected"; reason?: { kind: string; current?: DeviceState; state?: DeviceState; wanted?: string; code?: string; detail?: string } }

export const snapshot = signal<Snapshot | null>(null);
export const instances = computed<Instance[]>(() => {
  const s = snapshot();
  return s?.instances ?? (s && s.state !== "absent" ? [{ id: s.id, name: s.id, state: s.state }] : []);
});
export const selected = computed(() => {
  const s = snapshot();
  const id = s?.selectedId === undefined ? s?.id : s.selectedId;
  return instances().find(row => row.id === id) ?? null;
});
export const currentId = computed(() => selected()?.id ?? null);
export const preparation = computed(() => snapshot()?.preparation);
export const running = computed(() => (snapshot()?.running ?? []).filter(app => app.instanceId === currentId()));
export const environmentReady = computed(() => Boolean(snapshot()) && (!preparation() || (preparation()?.componentsReady ?? preparation()?.phase === "ready")));
export const images = computed(() => preparation()?.images ?? []);
export const activeTransfer = computed(() => ["downloading", "verifying", "installing"].includes(preparation()?.phase ?? ""));

export function pickResource(kind: "imageZip" | "runtimeDirectory" | "imageDirectory") {
  return host.call<string | null>("windroid.pickResource", { kind }, { timeout: 3_600_000 });
}
export const can = (key: Capability) => currentId() !== null && snapshot()?.capabilities?.[key] === true
  && (key !== "start" || environmentReady());

type Screen = "instances" | "library" | "manage" | "create" | "environment" | "settings" | "guide";
export const screen = signal<Screen>("instances");
export const instancePage = computed(() => screen() === "library" || screen() === "manage");
export const managementBack = signal<"library" | "instances">("instances");
type GuideOrigin = "first" | "instances" | "settings";
export const guideOrigin = signal<GuideOrigin>("instances");
export function openGuide(origin: GuideOrigin) {
  guideOrigin.set(origin);
  navigate("guide");
}
export const settingsTab = signal<"window" | "storage" | "logs">("window");
export function openStorage() {
  settingsTab.set("storage");
  navigate("settings");
}
export const environmentTab = signal<"components" | "images">("components");
export function openEnvironment(tab: "components" | "images") {
  environmentTab.set(tab);
  navigate("environment");
}
export const creationImage = signal<string | null>(null);
export function startCreation(imageId?: string) {
  creationImage.set(imageId ?? null);
  navigate("create");
}
let navigation = 0;
export function navigate(page: Screen) {
  ++navigation;
  screen.set(page);
}
export async function openInstance(id: string, page: "library" | "manage") {
  const request = ++navigation;
  const origin = screen();
  if (id !== currentId() && !await send({ kind: "selectInstance", instanceId: id }, "切换实例")) return;
  if (request !== navigation || currentId() !== id) return;
  if (page === "manage") managementBack.set(origin === "library" ? "library" : "instances");
  screen.set(page);
}
// 延迟到达的结果留在其发起实例。最近的全局消息和当前实例消息按到达顺序展示。
const messages = signal<Message[]>([]);
export const notice = computed(() => {
  const visible = messages().filter(item => item.instanceId === null || (instancePage() && item.instanceId === currentId()));
  return visible[visible.length - 1] ?? null;
});
export const loadError = signal("");
// 安装等设备操作只锁定发起它的实例。用户仍能切到另一台设备。
const requests = signal<Record<string, string>>({});
export const switching = computed(() => Boolean(requests().global || requests().selection));
export const pendingLabel = computed(() => requests().global || requests().selection || (instancePage() && requests()["instance:" + currentId()]) || "");
export const busy = computed(() => Boolean(pendingLabel()));

function remember(note: Message) {
  messages.update(items => [...items.filter(item => item.instanceId !== note.instanceId), note]);
}
export function report(message: string, failed: boolean, instanceId: string | null = null) {
  remember({ message, failed, instanceId });
}

// 运行状态、下载进度都会引发快照重读。只在宿主发布新结果时更新提示。
const lastOperations = new Map<string | null, number | string>();
function observeOperation(note?: Message | null) {
  if (!note) return;
  const key = note.seq ?? JSON.stringify(note);
  if (lastOperations.get(note.instanceId) === key) return;
  lastOperations.set(note.instanceId, key);
  remember(note);
}

// 事件只使快照失效。排队一次补读，防止较慢的旧响应覆盖新选择，也合并下载进度事件。
let reading: Promise<void> | null = null;
let dirty = false;
export function refresh(): Promise<void> {
  dirty = true;
  return reading ??= readChanges();
}
async function readChanges() {
  try {
    do {
      dirty = false;
      const s = await host.call<Snapshot>("windroid.snapshot");
      const first = !snapshot();
      snapshot.set(s);
      observeOperation(s.operation);
      if (first) {
        if (navigation === 0) {
          if (currentId()) screen.set("library");
          else if (s.showWelcome) openGuide("first");
        }
      }
      if (instancePage() && !currentId()) navigate("instances");
      loadError.set("");
    } while (dirty);
  } catch (error) {
    loadError.set("无法读取应用库 · " + String(error));
  } finally {
    reading = null;
  }
}

// 首次取到快照时路由仍在挂载。以首个工作台页面的原生布局回执确认已展示。
let firstPageSaved = false;
export function firstPageShown() {
  if (firstPageSaved || snapshot()?.showWelcome === undefined) return;
  firstPageSaved = true;
  void host.call("windroid.firstPageShown").catch(e => {
    firstPageSaved = false;
    report("未能保存首次进入记录 · " + String(e), true);
  });
}

// 受理和完成分开展示。异步完成、设备拒绝及窗口异常分别由宿主的 operation / Ack / streamNote 给出。
export async function send(cmd: Command, label: string): Promise<boolean> {
  const instanceId = "instanceId" in cmd ? cmd.instanceId : null;
  const key = cmd.kind === "selectInstance" ? "selection" : instanceId === null ? "global" : "instance:" + instanceId;
  if (switching() || requests()[key]) return false;
  requests.update(items => ({ ...items, [key]: label }));
  messages.update(items => items.filter(item => item.instanceId !== null && (cmd.kind === "selectInstance" || item.instanceId !== instanceId)));
  try {
    const ack = await host.call<Ack>("windroid.command", cmd, { timeout: cmd.kind === "applyStorage" ? 3_600_000 : 300_000 });
    if (ack?.status !== "accepted") {
      const r = ack?.reason;
      if (cmd.kind === "install" && r?.kind === "error" && r.code && r.detail !== undefined) {
        remember({ instanceId, failed: true, message: "APK 安装未完成", installError: { code: r.code, detail: r.detail } });
        return false;
      }
      const reason = r?.kind === "wrongState" && r.current
        ? "当前" + stateName[r.current] + (r.wanted ? "，需要" + r.wanted : "")
        : r?.kind === "notReady" ? "实例尚未就绪" : r?.detail || "宿主未提供原因";
      report(label + "失败 · " + reason, true, cmd.kind === "selectInstance" ? null : instanceId);
      return false;
    }
    // 清除数据没有独立的结果事件，回执只说明请求已提交。
    if (cmd.kind === "clearData") report("已提交清除数据请求", false, instanceId);
    return true;
  } catch (error) {
    report(label + "失败 · " + String(error), true, cmd.kind === "selectInstance" ? null : instanceId);
    return false;
  } finally {
    await refresh();
    requests.update(items => { const next = { ...items }; delete next[key]; return next; });
  }
}

/** 拖放仅接受本机的单个 APK。先验证整批再派发，避免部分安装或等待期间改投另一实例。 */
export function dropApk(uris: string[]) {
  const id = currentId();
  if (!id || !can("install")) { report("请先启动目标实例，再拖入 APK", true, id); return; }
  if (busy()) { report("请在当前操作完成后再拖入 APK", true, id); return; }
  if (uris.length !== 1) { report("每次请拖入一个 APK 文件", true, id); return; }
  try {
    const url = new URL(uris[0]);
    if (url.protocol !== "file:" || (url.hostname && url.hostname !== "localhost") || url.search || url.hash) throw new Error();
    const path = decodeURIComponent(url.pathname);
    if (!path.startsWith("/") || !path.toLowerCase().endsWith(".apk") || path.includes("\0")) throw new Error();
    void send({ kind: "install", instanceId: id, apkPath: path }, "安装 APK");
  } catch {
    report("请选择本机的 .apk 文件", true, id);
  }
}

export function connect() {
  host.on("windroid.event", () => { void refresh(); });
  void refresh();
}
