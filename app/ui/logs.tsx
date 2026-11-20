import { cleanup, computed, effect, host, signal, Text, View } from "nsh";
import { Menu } from "nsh/desktop";
import { Button, card, colors, Detail, Heading, row, Title } from "./controls";
import { currentId, instances, logOrigin, navigate, selected, takeRequestedLog } from "./model";

interface Stamp { local: string; day: string }
interface Run {
  id: string; started: Stamp; active: boolean; ended: Stamp | null; outcome: string | null;
  dates: string[]; lost: boolean; appRunId: string | null;
}
interface Catalog { runs: Run[]; legacy: boolean; issue: string | null; issues: string[] }
interface Entry {
  seq: number; stamp: Stamp; segment: number; source: string; level: string | null; component: string | null;
  text: string; continued: boolean; instanceId: string | null; emulatorRunId: string | null;
}
interface Page { records: Entry[]; more: boolean }
type Mode = "day" | "run" | "legacy";
interface Filter { mode: Mode; day: string; run: string }

const pageSize = 24;
const filters = new Map<string, Filter>();
// 离开再返回时恢复已读位置。缓存中只保存游标，记录内容随页面释放。
const cursors = new Map<string, number>();
function rememberCursor(key: string, after: number) {
  cursors.delete(key);
  cursors.set(key, after);
  if (cursors.size > 128) cursors.delete(cursors.keys().next().value!);
}

function read<T>(instanceId: string | null, query: Record<string, unknown>) {
  const owner = instanceId === null ? { source: "app" } : { source: "emulator", instanceId };
  return host.call<T>("windroid.logs", { ...owner, ...query });
}

function stampText(stamp: Stamp) {
  return stamp.local.replace("T", " ");
}
function metadata(entry: Entry) {
  const name = entry.instanceId ? instances().find(item => item.id === entry.instanceId)?.name : null;
  const source = [entry.source, entry.level, entry.component, entry.continued ? "续行" : "",
    entry.instanceId ? "实例 " + (name ? name + " · " : "") + entry.instanceId : "", entry.emulatorRunId ? "模拟器启动 " + entry.emulatorRunId : "",
  ].filter(Boolean).join(" · ");
  return "#" + entry.seq + "  " + stampText(entry.stamp) + "  " + source;
}

const stateText = (run: Run) => run.outcome ?? (run.active ? "采集中" : "退出原因未知");
const startLabel = (run: Run) => stampText(run.started) + " 启动";

function LogRun(props: {
  instanceId: string | null; run: Run; day: string | null; refresh: number; follow: boolean;
  pause: () => void; openDay: (day: string) => void; openRun: (run: string) => void;
}) {
  const instance = props.instanceId, runId = props.run.id, day = props.day;
  const key = JSON.stringify([instance, runId, day]);
  let after = cursors.get(key) ?? 0;
  let active = true;
  let pendingRefresh = false;
  const records = signal<Entry[]>([]);
  const more = signal(false);
  const working = signal(false);
  const loaded = signal(false);
  const error = signal("");
  const history = signal<number[]>([]);
  const atStart = signal(after === 0);
  cleanup(() => { active = false; });

  async function load(cursor: number, append = false, pages?: number[]) {
    if (working()) { pendingRefresh = true; return; }
    working.set(true);
    try {
      const page = await read<Page>(instance, { kind: "page", runId, day, after: cursor, limit: pageSize });
      if (!active) return;
      const merged = append ? [...records(), ...page.records] : page.records;
      const skip = Math.max(0, merged.length - pageSize);
      if (skip) after = merged[skip - 1].seq;
      else if (!append) after = cursor;
      records.set(merged.slice(skip));
      more.set(page.more);
      atStart.set(after === 0);
      rememberCursor(key, after);
      if (pages) history.set(pages);
      error.set("");
      loaded.set(true);
    } catch (e) { if (active) error.set("读取记录失败 · " + String(e)); }
    finally {
      working.set(false);
      if (active && pendingRefresh) { pendingRefresh = false; reload(); }
    }
  }
  function reload() {
    const tail = props.follow && loaded() && records().at(-1);
    void load(tail ? tail.seq : after, !!tail);
  }
  // 只在显式刷新或跟读节拍时请求，其他界面快照不会反复扫描日志。
  let lastRefresh = props.refresh;
  effect(() => { const tick = props.refresh; if (lastRefresh !== tick) { lastRefresh = tick; reload(); } });
  void load(after);

  const chunks = computed(() => {
    const groups: { day: string; segment: number; records: Entry[] }[] = [];
    for (const entry of records()) {
      const previous = groups.at(-1);
      if (previous && previous.segment === entry.segment && previous.day === entry.stamp.day) previous.records.push(entry);
      else groups.push({ day: entry.stamp.day, segment: entry.segment, records: [entry] });
    }
    return groups;
  });

  const identity = computed(() => "启动标识 · " + runId +
    (props.run.appRunId ? "\nWindroid 启动标识 · " + props.run.appRunId : ""));
  function pageNext() {
    const last = records().at(-1);
    if (!last || working()) return;
    props.pause();
    void load(last.seq, false, [...history(), after]);
  }
  function pageBack() {
    const pages = history();
    props.pause();
    void load(pages.at(-1) ?? 0, false, pages.slice(0, -1));
  }
  async function reveal(seq: number) {
    try { await read(instance, { kind: "reveal", runId, seq }); }
    catch (e) { if (active) error.set("无法定位日志段 · " + String(e)); }
  }

  return <View testID={"log-run-" + runId} style={card()}>
    <View style={row}>
      {day ? <Button id={"open-log-run-" + runId} label={startLabel(props.run)} accessibleValue={identity() + " · " + stateText(props.run)} press={() => props.openRun(runId)} /> :
        <Heading text={startLabel(props.run)} />}
      <Detail text={stateText(props.run)} accessible />
    </View>
    <Text role="text" accessibilityLabel={identity()} style={{ userSelect: "text", color: colors().muted, fontSize: 12 }}>{identity()}</Text>
    {props.run.lost ? <Detail text="部分记录已清理或采集有缺口。" accessible /> : null}
    <Each of={chunks()} key={g => g.records[0].seq}>{group => <View style={{ minWidth: 0, gap: 5 }}>
      <View style={row}>
        {!day ? <Button id={"open-log-day-" + group().records[0].seq} label={group().day} press={() => props.openDay(group().day)} /> : null}
        {group().segment > 0 ? <Button id={"log-file-" + runId + "-" + group().segment}
          label={"第 " + group().segment + " 段 · 在 Finder 中显示"}
          accessibleValue={[identity(), ...group().records.map(entry => metadata(entry) + "  " + entry.text)].join("\n")} press={() => void reveal(group().records[0].seq)} /> : null}
      </View>
      <Each of={group().records} key={entry => entry.seq}>{entry =>
        <View style={{ minWidth: 0, gap: 2, borderTopWidth: 1, borderColor: colors().line, paddingVertical: 5 }}>
          <Text role="text" accessibilityLabel={metadata(entry())} style={{ fontSize: 11, color: colors().muted, userSelect: "text" }}>{metadata(entry())}</Text>
          <Text role="text" accessibilityLabel={entry().text} testID={"log-line-" + runId + "-" + entry().seq} style={{ fontSize: 13, userSelect: "text",
            color: entry().source === "gap" || ["WARN", "ERROR"].includes(entry().level ?? "") ? colors().warn : colors().ink }}>{entry().text}</Text>
        </View>
      }</Each>
    </View>}</Each>
    {instance && loaded() && !error() && !day && !props.run.active && !props.run.lost && atStart() && !more()
      && !records().some(e => e.source === "stdout" || e.source === "stderr")
      ? <Detail text="本次启动未留存模拟器的标准输出或标准错误。" accessible /> : null}
    {error() ? <Text role="alert" style={{ color: colors().bad }}>{error()}</Text> :
      !loaded() ? <Detail text="正在读取记录…" /> : !records().length ? <Detail text={props.run.lost ? "此页没有保留的记录。" : "此页暂无输出。"} /> : null}
    <View style={row}>
      {!atStart() ? <Button id={"log-back-" + runId} label={history().length ? "上一页" : "回到开头"} disabled={working()} press={pageBack} /> : null}
      {more() ? <Button id={"log-next-" + runId} label="下一页" disabled={working()} press={pageNext} /> : null}
    </View>
  </View>;
}

function LegacyLog(props: { instanceId: string }) {
  const owner = props.instanceId;
  const page = signal({ text: "", next: 0, more: false });
  const offsets = signal<number[]>([0]);
  const error = signal("");
  const loading = signal(false);
  let active = true;
  cleanup(() => { active = false; });
  async function load(pages: number[]) {
    loading.set(true);
    try {
      const data = await read<ReturnType<typeof page>>(owner, { kind: "legacy", offset: pages.at(-1) });
      if (active) { page.set(data); offsets.set(pages); error.set(""); }
    } catch (e) { if (active) error.set("读取旧版日志失败 · " + String(e)); }
    finally { loading.set(false); }
  }
  async function reveal() {
    try { await read(owner, { kind: "revealLegacy" }); }
    catch (e) { if (active) error.set("无法定位旧版日志 · " + String(e)); }
  }
  void load([0]);
  return <View style={card()}>
    <Heading text="旧版日志 · 启动时间未知" />
    <View style={row}>
      <Button id="legacy-reveal" label="在 Finder 中显示" accessibleValue={page().text} press={reveal} />
      {offsets().length > 1 ? <Button id="legacy-previous" label="上一页" disabled={loading()} press={() => void load(offsets().slice(0, -1))} /> : null}
      {page().more ? <Button id="legacy-next" label="下一页" disabled={loading()} press={() => void load([...offsets(), page().next])} /> : null}
    </View>
    {error() ? <View style={row}><Text role="alert" style={{ color: colors().bad }}>{error()}</Text>
      <Button id="legacy-retry" label="重试" disabled={loading()} press={() => void load(offsets())} /></View> :
      <Text role="text" accessibilityLabel={page().text || "旧版日志"} testID="legacy-content" style={{ fontSize: 13, userSelect: "text", color: colors().ink }}>{page().text || (loading() ? "正在读取旧版日志…" : "文件为空。")}</Text>}
  </View>;
}

export function LogReader(props: { instanceId: string | null }) {
  const instance = props.instanceId;
  const owner = instance ?? "Windroid";
  const target = takeRequestedLog(instance);
  const initial: Filter = { mode: target ? "run" : "day", day: "", run: target?.run ?? "" };
  const filter = signal<Filter>(target ? initial : filters.get(owner) ?? initial);
  const catalog = signal<Catalog | null>(null);
  const loading = signal(false);
  const follow = signal(false);
  const revision = signal(0);
  const error = signal("");
  const groupLimit = signal(4);
  const runs = computed(() => catalog()?.runs ?? []);
  const days = computed(() => [...new Set(runs().flatMap(run => run.dates))].sort().reverse());
  const byDay = computed(() => filter().mode === "day");
  const visible = computed(() => byDay() ? runs().filter(run => run.dates.includes(filter().day)) : runs().filter(run => run.id === filter().run));
  let active = true;
  function choose(value: Partial<Filter>) {
    const next = { ...filter(), ...value };
    if (next.mode === "run" && !value.run && !runs().find(r => r.id === next.run)?.dates.includes(next.day))
      next.run = runs().find(r => r.dates.includes(next.day))?.id ?? next.run;
    if (value.mode === "day" && !value.day && filter().mode === "run") {
      const dates = runs().find(r => r.id === next.run)?.dates;
      if (dates?.length && !dates.includes(next.day)) next.day = dates[0];
    }
    filter.set(next);
    filters.delete(owner);
    filters.set(owner, next);
    if (filters.size > 64) filters.delete(filters.keys().next().value!);
    groupLimit.set(4);
  }
  async function refresh() {
    if (loading()) return;
    loading.set(true);
    try {
      const result = await read<Catalog>(instance, { kind: "catalog" });
      if (!active) return;
      catalog.set(result);
      const f = filter();
      if (!f.day || !f.run) choose({ day: f.day || days()[0] || "", run: f.run || result.runs[0]?.id || "" });
      revision.update(r => r + 1);
      error.set("");
    } catch (e) { if (active) error.set("读取日志目录失败 · " + String(e)); }
    finally { loading.set(false); }
  }
  async function select() {
    const choices = byDay() ? days().map(day => ({ label: day, checked: filter().day === day, onSelect: () => choose({ day }) })) :
      runs().map(run => ({ label: startLabel(run) + " · " + stateText(run) + " · " + run.id, checked: filter().run === run.id, onSelect: () => choose({ run: run.id }) }));
    try { await Menu.popup(choices); }
    catch (e) { if (active) error.set("无法显示日志列表 · " + String(e)); }
  }
  const timer = setInterval(() => { if (follow()) void refresh(); }, 2000);
  cleanup(() => { active = false; clearInterval(timer); });
  void refresh();
  return <View testID="log-reader" style={{ gap: 10 }}>
    <Heading text={instance === null ? "应用日志" : "启动与输出"} />
    <View style={row}>
      <Button id="logs-by-day" label="按日期" primary={byDay()} press={() => choose({ mode: "day" })} />
      <Button id="logs-by-run" label="按启动记录" primary={filter().mode === "run"} press={() => choose({ mode: "run" })} />
      {catalog()?.legacy ? <Button id="logs-legacy" label="旧版日志" primary={filter().mode === "legacy"} press={() => choose({ mode: "legacy" })} /> : null}
      {filter().mode !== "legacy" ? <Button id="logs-refresh" label="刷新" disabled={loading()} press={refresh} /> : null}
      {filter().mode !== "legacy" ? <Button id="logs-follow" label={follow() ? "暂停跟读" : "跟读"} press={() => { follow.set(!follow()); if (follow()) void refresh(); }} /> : null}
    </View>
    {error() ? <Text testID="logs-error" role="alert" style={{ color: colors().bad }}>{error()}</Text> : null}
    <Each of={catalog()?.issues} key={message => message}>{message =>
      <Text testID="log-history-issue" role="alert" accessibilityLabel={message()} style={{ userSelect: "text", color: colors().warn }}>{message()}</Text>
    }</Each>
    {catalog()?.issue ? <Text testID="capture-issue" role="alert" style={{ color: colors().warn }}>{"日志采集不可用或有缺口 · " + catalog()!.issue}</Text> : null}
    {filter().mode === "legacy" && instance ? <LegacyLog instanceId={instance} /> : <View style={{ gap: 12 }}>
      {(byDay() ? days().length : runs().length) ? <Button id="logs-choose" label={(byDay() ? filter().day || "选择日期" :
        runs().find(run => run.id === filter().run) ? startLabel(runs().find(run => run.id === filter().run)!) : "选择启动记录") + "  ▾"}
        accessibilityLabel={byDay() ? "选择日期" : "选择启动记录"} press={select} /> : null}
      <Each of={visible().slice(0, groupLimit()).map(run => ({ run, day: byDay() ? filter().day : null }))}
        key={g => g.run.id + "/" + g.day}>{group =>
        <LogRun instanceId={instance} run={group().run} day={group().day} refresh={revision()} follow={follow()}
          pause={() => follow.set(false)} openDay={day => choose({ mode: "day", day })} openRun={run => choose({ mode: "run", run })} />
      }</Each>
      {visible().length > groupLimit() ? <Button id="logs-more-runs" label="更多启动记录" press={() => groupLimit.update(n => n + 4)} /> : null}
      {!error() && catalog() && !visible().length ? <Detail id="logs-empty" accessible text={catalog()?.issues?.length ? "所选范围没有可读取的记录。" : target?.run && filter().run === target.run ? "此次启动没有可读取的持久化记录。" : runs().length ? "所选范围没有保留的记录。" :
        instance ? "暂无模拟器启动记录。启动前被拦截的原因可在「设置 → 应用日志」查看。" : "暂无应用启动记录。"} /> : null}
    </View>}
  </View>;
}

export function EmulatorLogs() {
  const owner = currentId();
  return <View style={{ gap: 12 }}>
    <Button id="log-return" label={logOrigin() === "manage" ? "返回管理实例" : "返回应用面板"} press={() => navigate(logOrigin())} />
    <Title text="模拟器日志" />
    <Detail text={(selected()?.name ?? "") + " · " + (owner ?? "")} accessible />
    {owner ? <LogReader instanceId={owner} /> : null}
  </View>;
}
