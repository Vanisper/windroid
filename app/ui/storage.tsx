import { cleanup, computed, host, signal, Text, View } from "nsh";
import { Button, card, colors, Detail, Heading, row } from "./controls";
import { busy, navigate, pickResource, refresh, send, snapshot } from "./model";
import { Location, size } from "./resources";

interface Roots { runtimeRoot: string; imageRoot: string | null }
interface Change extends Roots { expected: Roots }
interface Plan extends Roots {
  previewId: number; blockers: string[];
  files: { source: string; target: string; bytes: number }[];
}

// 用户可离开设置去停止实例。回来时重新读取文件与占用状态，原先选的目标目录保留。
const draft = signal<Change | null>(null);
const imagePath = (roots: Roots) => roots.imageRoot ?? roots.runtimeRoot + "/sdk/system-images";
const pageSize = 8;

export function Storage() {
  const current = computed(() => snapshot()?.storage);
  const base = computed<Roots | null>(() => {
    const storage = current();
    return storage ? { runtimeRoot: storage.runtimeRoot, imageRoot: storage.imagesInherited ? null : storage.images } : null;
  });
  const plan = signal<Plan | null>(null);
  const page = signal(0);
  const pending = signal("");
  const error = signal("");
  const locked = computed(() => !!pending() || busy());
  const files = computed(() => plan()?.files.slice(page() * pageSize, (page() + 1) * pageSize) ?? []);
  const totalBytes = computed(() => plan()?.files.reduce((sum, file) => sum + file.bytes, 0) ?? 0);
  const pageCount = computed(() => Math.ceil((plan()?.files.length ?? 0) / pageSize));
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
  async function preview(roots: Change) {
    draft.set(roots); error.set(""); pending.set("正在检查文件和占用情况…");
    try {
      const result = await host.call<Plan>("windroid.previewStorage", roots, { timeout: 300_000 });
      if (mounted) { page.set(0); plan.set(result); }
    } catch (e) {
      // 宿主发现另一进程已经改过位置时会重读设置，旧草案不能再用作新预览。
      await refresh();
      const stale = discardStale(roots);
      if (mounted) { plan.set(null); error.set(stale ? "存储位置已更改，请重新选择目标文件夹。" : "无法预览迁移 · " + String(e)); }
    } finally { if (mounted) pending.set(""); }
  }
  async function choose(kind: "runtimeDirectory" | "imageDirectory") {
    const expected = base();
    if (!expected || locked()) return;
    pending.set("正在选择文件夹…"); error.set("");
    try {
      const path = await pickResource(kind);
      if (!path || !mounted) return;
      await preview({
        expected,
        runtimeRoot: kind === "runtimeDirectory" ? path : expected.runtimeRoot,
        imageRoot: kind === "imageDirectory" ? path : expected.imageRoot,
      });
    } catch (e) {
      if (mounted) error.set("无法选择文件夹 · " + String(e));
    } finally { if (mounted) pending.set(""); }
  }
  function discard() { draft.set(null); plan.set(null); error.set(""); }
  async function apply() {
    const shown = plan();
    const change = draft();
    if (!change || !shown || shown.blockers.length || locked()) return;
    const ok = await send({ kind: "applyStorage", plan: { previewId: shown.previewId } }, "迁移存储位置");
    if (ok) {
      if (draft() === change) draft.set(null);
      if (mounted) plan.set(null);
    } else {
      const stale = discardStale(change);
      if (mounted) {
        plan.set(null);
        error.set(stale ? "存储位置已更改，请重新选择目标文件夹。" : "请查看上方失败原因。可以重新预览并重试。");
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
      <Button id="recover-storage" label="重新检查并恢复" disabled={locked()}
        press={() => send({ kind: "recheckRuntime" }, "恢复存储位置")} />
    </View> : null}
    <If when={plan()}>{review => <View testID="storage-preview" style={card()}>
      <Heading text="确认存储位置" />
      <Detail text="运行组件" /><Location path={draft()?.expected.runtimeRoot ?? ""} />
      <Location path={"→ " + review().runtimeRoot} />
      <Detail text="系统镜像" /><Location path={draft() ? imagePath(draft()!.expected) : ""} />
      <Location path={"→ " + imagePath(review()) + (review().imageRoot ? "" : "（跟随运行组件）")} />
      <Heading text={review().files.length ? "待移动的文件 · " + review().files.length + " 项 · " + size(totalBytes()) : "无需移动已有文件"} />
      <Each of={review().blockers} key={reason => reason}>{reason =>
        <Text role="alert" style={{ color: colors().warn, fontSize: 13 }}>{reason()}</Text>
      }</Each>
      <View style={row}>
        <Button id="storage-apply" label={review().files.length ? "移动并更改" : "确认更改"} primary
          disabled={locked() || !!review().blockers.length} press={apply} />
        <Button id="storage-recheck" label="重新预览" disabled={locked()} press={() => { if (draft()) void preview(draft()!); }} />
        {review().blockers.length ? <Button id="storage-see-instances" label="查看实例" press={() => navigate("instances")} /> : null}
        <Button id="cancel-storage" label="取消" disabled={locked()} press={discard} />
      </View>
      <Each of={files()} key={file => file.source}>{file => <View style={{ gap: 3, borderTopWidth: 1, borderColor: colors().line, paddingTop: 8 }}>
        <Location path={file().source} /><Location path={"→ " + file().target} /><Detail text={size(file().bytes)} />
      </View>}</Each>
      {pageCount() > 1 ? <View style={row}>
        <Button id="storage-prev" label="上一页" disabled={page() === 0} press={() => page.set(page() - 1)} />
        <Detail text={page() + 1 + " / " + pageCount()} />
        <Button id="storage-next" label="下一页" disabled={page() + 1 >= pageCount()} press={() => page.set(page() + 1)} />
      </View> : null}
    </View>}</If>
    {!plan() ? <View style={card()}>
      <Heading text="主数据目录" />
      <Location path={current()?.home ?? "未知"} />
      <Detail text="实例的私有数据、设置和日志保存在此目录。" />
      <Heading text="运行组件目录" />
      <Location path={current()?.runtimeRoot ?? "未知"} />
      {current()?.borrowed ? <Detail text={"使用开发者指定的 SDK · " + current()!.sdk} /> : null}
      <Button id="choose-runtime-directory" label="更改运行组件位置…" disabled={locked() || !current() || current()?.borrowed}
        press={() => choose("runtimeDirectory")} />
      <Heading text="系统镜像目录" />
      <Location path={current()?.images ?? "未知"} />
      {current() ? <Detail text={current()!.borrowed ? "由开发者环境管理" : current()!.imagesInherited ? "跟随运行组件" : "单独存放"} /> : null}
      <View style={row}>
        <Button id="choose-image-directory" label="单独选择镜像位置…" disabled={locked() || !current() || current()?.borrowed}
          press={() => choose("imageDirectory")} />
        {current() && !current()!.imagesInherited && !current()!.borrowed ? <Button id="inherit-image-directory" label="恢复跟随运行组件" disabled={locked()}
          press={() => { const expected = base(); if (expected) void preview({ ...expected, imageRoot: null, expected }); }} /> : null}
        {draft() && !pending() ? <Button id="retry-storage-preview" label="重试上次迁移预览" disabled={locked()}
          press={() => { if (draft()) void preview(draft()!); }} /> : null}
      </View>
    </View> : null}
  </View>;
}
