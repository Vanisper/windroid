import { cleanup, computed, host, Scroll, signal, Text, View, windowSize } from "nsh";
import { Checkbox, Dialog } from "nsh/base";
import { Button, card, colors, Detail, Heading, row } from "./controls";
import { instances, report, send, snapshot, switching, type BatchAction, type BatchScope } from "./model";

interface Preview {
  previewId: number; action: BatchAction; scope: BatchScope;
  items: { id: string; name: string; eligible: boolean; reason: string; windows: string[] }[];
}
type PreviewRequest = { action: BatchAction; scope: "all" | "selected"; ids: string[] } | { previewId: number } | { retryOf: number };
const verb = { start: "启动", stop: "停止" };
const scopeLabel = { all: "本机全部", selected: "已选", retry: "上次失败的" };

// 选择属于全部实例页面，离开查看单台设置后仍可恢复。命令按当前列表顺序取 ID。
const selection = signal<string[]>([]);
const selectedIds = computed(() => instances().filter(item => selection().includes(item.id)).map(item => item.id));
const dismissed = signal<number | null>(null);
export const visibleJob = computed(() => {
  const job = snapshot()?.batch;
  return job && job.jobId !== dismissed() ? job : null;
});

export function SelectInstance(props: { id?: string; name?: string }) {
  const checked = computed(() => props.id ? selectedIds().includes(props.id) : selectedIds().length > 0 && selectedIds().length === instances().length);
  const mixed = computed(() => !props.id && selectedIds().length > 0 && !checked());
  const marked = computed(() => checked() || mixed());
  function change(on: boolean) {
    const id = props.id;
    if (id) selection.set(on ? [...selectedIds(), id] : selectedIds().filter(item => item !== id));
    else selection.set(on ? instances().map(item => item.id) : []);
  }
  return <Checkbox testID={props.id ? "check-" + props.id : "check-all"} accessibilityLabel={props.name ? "选择 " + props.name : "选择全部实例"}
    checked={checked()} indeterminate={mixed()} onChange={event => change(event.checked)} hitSlop={3}
    style={{ flexDirection: "row", alignItems: "center", minHeight: 26, gap: 8, paddingHorizontal: 2, borderRadius: 4 }}
    focusVisibleStyle={{ backgroundColor: colors().selected }}
    indicatorStyle={{ width: 17, height: 17, alignItems: "center", justifyContent: "center", borderWidth: 1, borderRadius: 4,
      borderColor: marked() ? colors().accent : colors().muted, backgroundColor: marked() ? colors().accent : colors().card }}
    indicator={<Text style={{ fontSize: 12, fontWeight: 700, color: colors().onAccent }}>{mixed() ? "−" : checked() ? "✓" : ""}</Text>}>
    {!props.id ? <Detail text="全选" /> : null}
  </Checkbox>;
}

export function BatchActions() {
  const preview = signal<Preview | null>(null);
  const checking = signal(false);
  const eligible = computed(() => preview()?.items.filter(item => item.eligible).length ?? 0);
  const locked = computed(() => checking() || switching());
  const failures = computed(() => visibleJob()?.items.filter(item => item.status === "failed").length ?? 0);
  const summary = computed(() => {
    const job = visibleJob();
    if (!job) return selectedIds().length ? "已选 " + selectedIds().length + " 个实例" : "共 " + instances().length + " 个实例";
    const ok = job.items.filter(item => item.status === "succeeded").length;
    const skipped = job.items.filter(item => item.status === "skipped").length;
    return job.active ? "正在" + verb[job.action] + " · " + (ok + skipped + failures()) + " / " + job.items.length :
      "成功 " + ok + " 个 · 跳过 " + skipped + " 个 · 失败 " + failures() + " 个";
  });
  let mounted = true;
  cleanup(() => { mounted = false; });
  async function inspect(request: PreviewRequest) {
    if (locked()) return;
    checking.set(true);
    try {
      const result = await host.call<Preview>("windroid.previewBatch", request, { timeout: 300_000 });
      if (mounted) preview.set(result);
    } catch (error) {
      if (mounted) {
        preview.set(null);
        report("无法检查批量操作 · " + String(error), true);
      }
    } finally { if (mounted) checking.set(false); }
  }
  function begin(action: BatchAction) {
    if (visibleJob() || preview()) return;
    const chosen = selectedIds();
    void inspect({ action, scope: chosen.length ? "selected" : "all", ids: chosen.length ? chosen : instances().map(item => item.id) });
  }
  function commit() {
    const shown = preview();
    if (!shown || !eligible() || locked()) return;
    preview.set(null);
    void send({ kind: "executeBatch", previewId: shown.previewId }, "批量" + verb[shown.action]);
  }
  function finish() {
    const job = visibleJob();
    if (job && !job.active) { dismissed.set(job.jobId); selection.set([]); }
  }

  const hasTargets = computed(() => instances().length > 0 || !!visibleJob());
  return <View style={{ gap: 6 }}>
    {hasTargets() ? <View style={{ ...row, justifyContent: "space-between" }}>
      <Detail accessible id="batch-summary" text={checking() ? "正在检查目标实例…" : summary()} />
      {visibleJob() ? <View style={row}>
        {!visibleJob()!.active && failures() ? <Button id="batch-retry" label="重试失败项…" disabled={locked()}
          press={() => { const job = visibleJob(); if (job && !job.active) void inspect({ retryOf: job.jobId }); }} /> : null}
        {!visibleJob()!.active ? <Button id="batch-done" label="完成" disabled={locked()} press={finish} /> : null}
      </View> : <View style={row}>
        <Button id="batch-start" label={selectedIds().length ? "启动所选…" : "全部启动…"} disabled={locked()} press={() => begin("start")} />
        <Button id="batch-stop" label={selectedIds().length ? "停止所选…" : "全部停止…"} disabled={locked()} press={() => begin("stop")} />
      </View>}
    </View> : null}
    <If when={preview()}>{shown => <Dialog open animation="fade" role="alertdialog" accessibilityLabel={"确认批量" + verb[shown().action]}
      onCancel={() => { if (!checking()) preview.set(null); }} closeOnEscape={!checking()} closeOnBackdropPress={!checking()}
      backdropStyle={{ backgroundColor: "#0A170F70" }}
      style={{ ...card(), margin: "auto", width: Math.min(460, windowSize().width - 32), maxHeight: windowSize().height - 32, padding: 18, gap: 12 }}>
      <Heading text={"确认批量" + verb[shown().action]} />
      <Detail accessible text={scopeLabel[shown().scope] + " " + shown().items.length + " 个实例 · 可" + verb[shown().action] + " " + eligible() + " 个"} />
      <Scroll testID="batch-targets" style={{ minHeight: 0, flexShrink: 1, maxHeight: Math.max(100, Math.floor(windowSize().height * 0.45)) }}>
        <Each of={shown().items} key={item => item.id}>{item => <View testID={"preview-" + item().id} accessible role="text"
          accessibilityLabel={item().name + " · " + (item().eligible ? "" : "跳过 · ") + item().reason +
            (item().eligible && shown().action === "stop" && item().windows.length ? " · 将关闭 " + item().windows.join("、") : "")}
          style={{ gap: 4, paddingVertical: 8, borderTopWidth: 1, borderColor: colors().line }}>
          <Text style={{ fontSize: 14, fontWeight: 600, color: colors().ink }}>{item().name}</Text>
          <Detail text={(item().eligible ? "" : "跳过 · ") + item().reason} />
          {item().eligible && shown().action === "stop" && item().windows.length ?
            <Detail text={"将关闭 · " + item().windows.join("、")} /> : null}
        </View>}</Each>
      </Scroll>
      {eligible() ? <Detail accessible text={shown().action === "start" ? "启动前会逐台重新检查可用资源和状态。" :
        "将关闭可停止实例的应用窗口和设备视图，并结束这些实例的后台任务。"} /> : null}
      <View style={row}>
        <Button id="batch-cancel" label={eligible() ? "取消" : "完成"} disabled={locked()} press={() => preview.set(null)} />
        <Button id="batch-recheck" label={checking() ? "正在检查…" : "重新检查"} disabled={locked()}
          press={() => void inspect({ previewId: shown().previewId })} />
        {eligible() ? <Button id="batch-confirm" label={verb[shown().action] + " " + eligible() + " 个"} primary
          disabled={locked()} press={commit} /> : null}
      </View>
    </Dialog>}</If>
  </View>;
}
