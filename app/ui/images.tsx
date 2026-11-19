import { cleanup, computed, effect, host, signal, Text, View } from "nsh";
import { Button, card, colors, Detail, Heading, row } from "./controls";
import { activeTransfer, busy, environmentReady, images, pickResource, preparation, send, startCreation, type Component } from "./model";
import { LicenseLink, Location, Progress, size } from "./resources";

export function imageDescription(item: Component) {
  const image = item.image;
  return image ? "API " + image.api + " · " + (image.tag === "google_apis_playstore" ? "Google Play" : image.tag)
    + " · " + image.abi + " · 修订版 " + item.version : "修订版 " + item.version;
}

function ImageRow(props: { item: Component; select?: (id: string) => void; download: (item: Component) => void }) {
  const phase = computed(() => preparation()?.currentComponent === props.item.id ? preparation()?.phase : null);
  const active = computed(() => !!phase() && activeTransfer());
  const error = computed(() => phase() === "failed" ? preparation()?.error : null);
  const downloadLabel = computed(() => {
    const action = phase() === "failed" ? "重试" : props.item.downloadedBytes > 0 || phase() === "paused" ? "继续下载" : "下载";
    return props.select ? "同意" + action + "并选用" : "同意并" + action;
  });
  return <View testID={"image-" + props.item.id} style={card()}>
    <View style={row}>
      <Heading text={props.item.name} />
      <Detail text={size(props.item.sizeBytes)} />
    </View>
    <Detail text={imageDescription(props.item)} />
    {props.item.minEmulator ? <Detail text={"需要 Android Emulator " + props.item.minEmulator + " 或更高版本"} /> : null}
    <Location path={props.item.location} />
    {props.item.issue ? <Detail text={props.item.issue} /> : null}
    {active() || (!props.item.ready && props.item.downloadedBytes > 0) ? <View style={{ gap: 5 }}>
      <Detail text={phase() === "installing" ? "正在安装镜像" : phase() === "verifying" ? "正在校验镜像" :
        active() ? "正在准备镜像" : "已保留下载进度"} />
      <Progress id={"image-progress-" + props.item.id} downloaded={props.item.downloadedBytes} total={props.item.sizeBytes} />
    </View> : null}
    {error() ? <Text role="alert" style={{ color: colors().bad }}>{error()}</Text> : null}
    <View style={row}>
      <LicenseLink item={props.item} />
      {props.item.ready ? <Button id={"use-" + props.item.id} primary disabled={!environmentReady() || busy()}
        label={props.select ? "使用此镜像" : "用于新实例"} accessibilityLabel={"使用 " + props.item.name + " 创建实例"}
        press={() => (props.select ?? startCreation)(props.item.id)} /> :
        props.item.borrowed ? <Detail text="请在指定的 SDK 中准备此镜像，然后重新检查。" /> :
        active() ? (phase() === "downloading" ? <Button id={"pause-" + props.item.id} label="暂停下载" disabled={busy()}
          press={() => send({ kind: "pausePreparation" }, "暂停镜像下载")} /> : null) :
        <Button id={"download-" + props.item.id} label={downloadLabel()} primary disabled={activeTransfer() || busy()}
          accessibilityLabel={downloadLabel() + " " + props.item.name}
          press={() => props.download(props.item)} />}
    </View>
  </View>;
}

export function ImageList(props: { select?: (id: string) => void }) {
  const versions = computed(() => [...images()].sort((a, b) => (b.image?.api ?? 0) - (a.image?.api ?? 0)));
  const local = computed(() => versions().filter(c => c.ready));
  const remote = computed(() => versions().filter(c => !c.ready && !c.borrowed));
  const external = computed(() => versions().filter(c => !c.ready && c.borrowed));
  const inspected = signal<{ path: string; id: string } | null>(null);
  const candidate = computed(() => images().find(i => i.id === inspected()?.id));
  const awaiting = signal<string | null>(null);
  // 只有从当前创建流程明确选用的镜像才推进表单。退出后下载仍由宿主负责。
  effect(() => {
    const id = awaiting();
    if (id && local().some(i => i.id === id)) {
      awaiting.set(null);
      props.select?.(id);
    }
  });
  const status = signal("");
  const error = signal("");
  let visible = true;
  cleanup(() => { visible = false; });
  async function download(item: Component) {
    if (props.select) awaiting.set(item.id);
    if (!await send({ kind: "prepareImage", imageId: item.id, acceptedLicenses: true }, "准备 " + item.name) && visible) awaiting.set(null);
  }
  async function inspect() {
    if (status()) return;
    error.set(""); inspected.set(null); status.set("正在选择 ZIP 文件…");
    try {
      const path = await pickResource("imageZip");
      if (!path || !visible) return;
      status.set("正在校验官方完整 ZIP…");
      const { imageId } = await host.call<{ imageId: string }>("windroid.inspectImage", { path }, { timeout: 600_000 });
      if (!visible) return;
      if (!images().some(i => i.id === imageId)) throw new Error("当前清单没有与此 ZIP 匹配的镜像");
      inspected.set({ path, id: imageId });
    } catch (e) {
      if (visible) error.set("无法导入此镜像 · " + String(e));
    } finally { if (visible) status.set(""); }
  }
  async function confirm() {
    const result = inspected();
    if (!result || !candidate()) return;
    if (candidate()!.ready) {
      inspected.set(null); props.select?.(result.id);
    } else {
      if (props.select) awaiting.set(result.id);
      const ok = await send({ kind: "importImage", imageId: result.id, path: result.path, acceptedLicenses: true }, "导入系统镜像");
      if (visible) {
        if (ok) inspected.set(null);
        else awaiting.set(null);
      }
    }
  }
  return <View testID="image-list" style={{ gap: 12 }}>
    <View style={row}>
      <Heading text={props.select ? "选择系统镜像" : "系统镜像"} />
      <Button id="pick-image-zip" label="导入本地镜像…" disabled={!!status() || activeTransfer() || busy()} press={inspect} />
    </View>
    <Detail text={preparation()?.borrowed ? "正在使用开发者指定的 SDK。此处可选用该 SDK 中已就绪的镜像。" :
      "可下载或导入清单中的官方完整 ZIP。多个实例可使用同一镜像，各自保存应用和文件。"} />
    {status() ? <Detail id="image-checking" text={status()} /> : null}
    {error() ? <Text testID="import-error" role="alert" style={{ userSelect: "text", color: colors().bad }}>{error()}</Text> : null}
    <If when={candidate()}>{item => <View testID="image-import-preview" style={card()}>
      <Heading text={item().ready ? "本机已有此镜像" : "已与官方清单匹配"} />
      <Location path={inspected()?.path ?? ""} />
      <Detail text={item().name + " · " + imageDescription(item())} />
      <Detail text={"存储位置 · " + item().location} />
      {item().minEmulator ? <Detail text={"需要 Android Emulator " + item().minEmulator + " 或更高版本"} /> : null}
      {!item().ready && item().borrowed ? <Detail text="请在指定的 SDK 中安装此版本，然后重新检查。" /> : null}
      <View style={row}>
        <LicenseLink item={item()} id="import-license" />
        <Button id="confirm-import" primary disabled={busy() || activeTransfer() || (!item().ready && item().borrowed)}
          label={item().ready ? props.select ? "使用此镜像" : "完成" : props.select ? "同意导入并选用" : "同意并导入"} press={confirm} />
        <Button id="cancel-import" label="取消" press={() => inspected.set(null)} />
      </View>
    </View>}</If>
    <Heading text="本机可用" />
    {!local().length ? <Detail text="还没有可用的系统镜像。" /> : null}
    <Each of={local()} key={item => item.id}>{item => <ImageRow item={item()} select={props.select} download={download} />}</Each>
    {remote().length ? <Heading text="可下载" /> : null}
    <Each of={remote()} key={item => item.id}>{item => <ImageRow item={item()} select={props.select} download={download} />}</Each>
    {external().length ? <Heading text="需在指定的 SDK 中准备" /> : null}
    <Each of={external()} key={item => item.id}>{item => <ImageRow item={item()} select={props.select} download={download} />}</Each>
  </View>;
}
