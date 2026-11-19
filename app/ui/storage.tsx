import { cleanup, computed, host, signal, Text, View } from "nsh";
import { Button, card, colors, Detail, Heading, row } from "./controls";
import { activeTransfer, busy, images, navigate, pickResource, preparation, refresh, send, snapshot } from "./model";
import { LicenseLink, Location, Progress, size } from "./resources";

interface Roots { runtimeRoot: string; imageRoot: string }
type Mode = "future" | "use" | "migrate";
interface Change extends Roots { expected: Roots; mode: Mode; cleanup: boolean }
interface Version {
  id: string; name: string; source: string; target: string; bytes: number;
  status: "reuse" | "copy" | "missing" | "conflict" | "unused";
  detail: string; cleanup: boolean; downloadable: boolean;
  downloadRequiredBytes: number | null; downloadAvailableBytes: number | null; downloadWritable: boolean;
}
interface Plan extends Roots {
  previewId: number; mode: Mode; cleanup: boolean; blockers: string[]; versions: Version[];
  affectedInstances: string[]; requiredBytes: number; availableBytes: number | null;
}

const modes: { id: Mode; label: string }[] = [
  { id: "use", label: "使用现有目录" },
  { id: "future", label: "设置下载位置" },
  { id: "migrate", label: "迁移已有版本" },
];
const status = { reuse: "可原地复用", copy: "需要复制", missing: "目标缺少版本", conflict: "内容冲突", unused: "无需处理" };
// 用户可离开设置去停止实例。回来时重新读取目标、依赖、占用情况和上次选定的操作。
const draft = signal<Change | null>(null);
const pageSize = 6;

function VersionRow(props: { item: Version; previewId: number; locked: boolean }) {
  const item = computed(() => images().find(i => i.id === props.item.id));
  const job = computed(() => preparation()?.candidateTarget === props.item.target && preparation()?.currentComponent === props.item.id);
  const inProgress = computed(() => job() && activeTransfer());
  const accepted = signal<number | null>(null);
  const completed = computed(() => job() && preparation()?.phase === "ready" && accepted() === props.previewId);
  const cannotDownload = computed(() => !props.item.downloadWritable ? "目标目录不可写" :
    props.item.downloadRequiredBytes !== null && props.item.downloadAvailableBytes !== null &&
    props.item.downloadRequiredBytes > props.item.downloadAvailableBytes ? "目标卷空间不足" : "");
  async function start() {
    const id = props.previewId;
    accepted.set(id);
    if (!await send({ kind: "prepareCandidateImage", previewId: id,
      resourceId: props.item.id, acceptedLicenses: true }, "准备目标版本")) accepted.set(null);
  }
  return <View testID={"storage-version-" + props.item.id}
    style={{ borderTopWidth: 1, borderColor: colors().line, paddingTop: 10, gap: 5 }}>
    <View style={row}><Heading text={props.item.name} /><Detail text={status[props.item.status]} /></View>
    <Location path={"原位置 · " + props.item.source} /><Location path={"目标位置 · " + props.item.target} />
    {props.item.status === "copy" ? <Detail text={"复制大小 · " + size(props.item.bytes)} /> : null}
    {props.item.detail ? <Location path={props.item.detail} /> : null}
    {props.item.cleanup ? <Detail text="切换成功后，复核并清理此源版本。" /> : null}
    {(props.item.downloadable || inProgress()) && item() ? <View style={{ gap: 6 }}>
      <Detail text={"官方镜像 ZIP · " + size(item()!.sizeBytes)} />
      {props.item.downloadRequiredBytes != null ? <Detail text={"下载及安装需额外空间 · " + size(props.item.downloadRequiredBytes) +
        (props.item.downloadAvailableBytes === null ? "    可用空间未知" : "    可用 · " + size(props.item.downloadAvailableBytes))} /> : null}
      {props.item.downloadable && cannotDownload() ? <Detail text={cannotDownload()} /> : null}
      {job() ? <View style={{ gap: 5 }}>
        {preparation()?.phase === "failed" ? <Detail text={preparation()?.error ?? "下载失败"} /> : null}
        <Progress id={"candidate-progress-" + props.item.id} downloaded={preparation()?.candidateDownloadedBytes ?? 0} total={item()!.sizeBytes} />
      </View> : null}
      <View style={row}>
        <LicenseLink item={item()!} />
        {inProgress() ? preparation()?.phase === "downloading" ?
          <Button id="pause-candidate" label="暂停下载" disabled={props.locked}
            press={() => send({ kind: "pausePreparation" }, "暂停目标镜像下载")} /> : <Detail text="正在校验并安装目标镜像…" /> :
          completed() ? <Detail text="目标镜像已准备，请重新预览。" /> : <Button id={"download-candidate-" + props.item.id} label={job() && preparation()?.phase === "paused" ? "同意并继续下载到目标" : "同意并下载到目标"}
            disabled={props.locked || activeTransfer() || !!cannotDownload()} press={start} />}
      </View>
      <Detail text="下载完成后重新预览，再确认目录切换。" />
    </View> : null}
  </View>;
}

export function Storage() {
  const current = computed(() => snapshot()?.storage);
  const base = computed<Roots | null>(() => {
    const storage = current();
    return storage ? { runtimeRoot: storage.runtimeRoot, imageRoot: storage.imageRoot } : null;
  });
  const plan = signal<Plan | null>(null);
  const page = signal(0);
  const pending = signal("");
  const error = signal("");
  const locked = computed(() => !!pending() || busy());
  const versions = computed(() => plan()?.versions.slice(page() * pageSize, (page() + 1) * pageSize) ?? []);
  const pageCount = computed(() => Math.ceil((plan()?.versions.length ?? 0) / pageSize));
  let mounted = true;
  cleanup(() => { mounted = false; });

  function discardStale(change: Change) {
    const now = base();
    if (now && (change.expected.runtimeRoot !== now.runtimeRoot || change.expected.imageRoot !== now.imageRoot)) {
      if (draft() === change) draft.set(null);
      return true;
    }
    return false;
  }
  async function preview(change: Change) {
    draft.set(change); error.set(""); pending.set("正在核验版本和实例依赖…");
    try {
      const result = await host.call<Plan>("windroid.previewStorage", change, { timeout: 3_600_000 });
      if (mounted) { page.set(0); plan.set(result); }
    } catch (e) {
      await refresh();
      const stale = discardStale(change);
      if (mounted) { plan.set(null); error.set(stale ? "存储位置已更改，请重新选择目标文件夹。" : "无法预览目录 · " + String(e)); }
    } finally { if (mounted) pending.set(""); }
  }
  async function choose(key: keyof Roots, restore = false) {
    const expected = base();
    const storage = current();
    if (!expected || !storage || locked()) return;
    pending.set("正在选择文件夹…"); error.set("");
    try {
      const path = restore ? (key === "runtimeRoot" ? storage.defaultRuntimeRoot : storage.defaultImageRoot) :
        await pickResource(key === "runtimeRoot" ? "runtimeDirectory" : "imageDirectory");
      if (!path || !mounted) return;
      await preview({ ...expected, [key]: path, expected, mode: "use", cleanup: false });
    } catch (e) {
      if (mounted) error.set("无法选择文件夹 · " + String(e));
    } finally { if (mounted) pending.set(""); }
  }
  function changeMode(mode: Mode, cleanup = false) {
    const change = draft();
    if (change && !locked()) void preview({ ...change, mode, cleanup });
  }
  function discard() { draft.set(null); plan.set(null); error.set(""); }
  async function apply() {
    const shown = plan();
    const change = draft();
    if (!change || !shown || shown.blockers.length || locked()) return;
    const ok = await send({ kind: "applyStorage", plan: { previewId: shown.previewId } }, "更改存储位置");
    if (ok) {
      if (draft() === change) draft.set(null);
      if (mounted) plan.set(null);
    } else {
      const stale = discardStale(change);
      if (mounted) {
        plan.set(null);
        error.set(stale ? "存储位置已更改，请重新选择目标文件夹。" : "请查看上方失败原因，然后重新预览。");
      }
    }
  }
  const resume = draft();
  if (resume) void preview(resume);
  return <View testID="storage-panel" style={{ gap: 12 }}>
    {pending() ? <Detail text={pending()} /> : null}
    {error() ? <Text role="alert" style={{ userSelect: "text", color: colors().bad }}>{error()}</Text> : null}
    {current()?.error ? <View style={card()}>
      <Heading text="存储位置需要恢复" /><Detail text={current()!.error!} />
      <Button id="recover-storage" label="重新检查并恢复" disabled={locked()} press={() => send({ kind: "recheckRuntime" }, "恢复存储位置")} />
    </View> : null}
    {current()?.cleanupNotes?.length ? <View style={card()}>
      <Heading text="原位置清理" />
      <Each of={current()?.cleanupNotes} key={note => note}>{note => <Location path={note()} />}</Each>
      <Button id="review-source-cleanup" label="复核源版本" disabled={locked()} press={() => send({ kind: "recheckRuntime" }, "复核源版本")} />
    </View> : null}
    <If when={plan()}>{review => <View testID="storage-preview" style={card()}>
      <Heading text="确认存储位置" />
      {review().runtimeRoot !== draft()?.expected.runtimeRoot ? <View style={{ gap: 4 }}>
        <Detail text="运行组件目录" /><Location path={draft()?.expected.runtimeRoot ?? ""} /><Location path={"→ " + review().runtimeRoot} />
      </View> : null}
      {review().imageRoot !== draft()?.expected.imageRoot ? <View style={{ gap: 4 }}>
        <Detail text="系统镜像目录" /><Location path={draft()?.expected.imageRoot ?? ""} /><Location path={"→ " + review().imageRoot} />
      </View> : null}
      <View style={row}><Each of={modes} key={m => m.id}>{m =>
        <Button id={"storage-mode-" + m().id} primary={review().mode === m().id} label={m().label}
          disabled={locked() || review().mode === m().id} press={() => changeMode(m().id)} />
      }</Each></View>
      <Detail text={review().mode === "future" ? "在此目录保存今后确认下载的版本。" :
        review().mode === "use" ? "完整核验目标的现有版本，确认后从这里读取。" : "同一版本内容一致时复用，目标缺失的版本从源目录复制。"} />
      {review().mode === "migrate" ? <View style={{ gap: 6 }}>
        <Detail text={review().cleanup ? "清理源版本可能影响仍从原位置读取的其他工作区。" : "切换后保留源版本。"} />
        <Button id="storage-toggle-cleanup" disabled={locked()} label={review().cleanup ? "改为保留源版本" : "选择清理已迁移的源版本…"}
          press={() => changeMode("migrate", !review().cleanup)} />
      </View> : null}
      <Detail text={"目标需额外空间 · " + size(review().requiredBytes) +
        (review().availableBytes === null ? "" : "    可用 · " + size(review().availableBytes!))} />
      {review().affectedInstances.length ? <Heading text="受影响的实例" /> : null}
      <Each of={review().affectedInstances} key={name => name}>{name => <Detail text={name()} />}</Each>
      <Each of={review().blockers} key={reason => reason}>{reason =>
        <Text role="alert" style={{ userSelect: "text", color: colors().warn, fontSize: 13 }}>{reason()}</Text>
      }</Each>
      <View style={row}>
        <Button id="storage-apply" label={review().mode === "future" ? "确认下载位置" : review().mode === "use" ? "使用此目录" :
          review().cleanup ? "迁移并清理已确认源版本" : "迁移并保留源版本"} primary disabled={locked() || !!review().blockers.length} press={apply} />
        <Button id="storage-recheck" label="重新预览" disabled={locked()} press={() => { if (draft()) void preview(draft()!); }} />
        {review().affectedInstances.length ? <Button id="storage-see-instances" label="查看实例" press={() => navigate("instances")} /> : null}
        <Button id="cancel-storage" label="取消" disabled={locked()} press={discard} />
      </View>
      <Each of={versions()} key={v => v.id}>{v => <VersionRow item={v()} previewId={review().previewId} locked={locked()} />}</Each>
      {pageCount() > 1 ? <View style={row}>
        <Button id="storage-prev" label="上一页" disabled={page() === 0} press={() => page.set(page() - 1)} />
        <Detail text={page() + 1 + " / " + pageCount()} />
        <Button id="storage-next" label="下一页" disabled={page() + 1 >= pageCount()} press={() => page.set(page() + 1)} />
      </View> : null}
    </View>}</If>
    {!plan() ? <View style={card()}>
      {preparation()?.candidateTarget && activeTransfer() ? <View style={{ gap: 5 }}>
        <Heading text="正在准备目标镜像" /><Location path={preparation()!.candidateTarget!} />
        {preparation()?.phase === "downloading" ? <Button id="pause-storage-download" label="暂停下载" disabled={locked()}
          press={() => send({ kind: "pausePreparation" }, "暂停目标镜像下载")} /> : null}
      </View> : null}
      <Heading text="工作区" /><Location path={current()?.home ?? "未知"} />
      <Detail text="实例的私有数据、设置和日志保存在这里。" />
      <Heading text="运行组件目录" /><Location path={current()?.runtimeRoot ?? "未知"} />
      <View style={row}>
        <Button id="choose-runtime-directory" label="选择组件文件夹…" disabled={locked() || !current()} press={() => choose("runtimeRoot")} />
        {current()?.runtimeIsDefault === false ? <Button id="reset-runtime-directory"
          label="恢复默认组件目录" disabled={locked()} press={() => choose("runtimeRoot", true)} /> : null}
      </View>
      <Heading text="系统镜像目录" /><Location path={current()?.imageRoot ?? "未知"} />
      <View style={row}>
        <Button id="choose-image-directory" label="选择镜像文件夹…" disabled={locked() || !current()} press={() => choose("imageRoot")} />
        {current()?.imageIsDefault === false ? <Button id="reset-image-directory"
          label="恢复默认镜像目录" disabled={locked()} press={() => choose("imageRoot", true)} /> : null}
      </View>
      {draft() && !pending() ? <Button id="retry-storage-preview" label="重试上次预览" disabled={locked()}
        press={() => { if (draft()) void preview(draft()!); }} /> : null}
    </View> : null}
  </View>;
}
