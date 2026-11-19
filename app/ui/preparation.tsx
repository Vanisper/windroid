import { computed, Text, View } from "nsh";
import { Button, card, colors, Detail, Heading, row, Title } from "./controls";
import { ImageList } from "./images";
import { activeTransfer, busy, environmentReady, environmentTab, navigate, preparation, send, type Component, type Preparation } from "./model";
import { LicenseLink, Location, Progress, size } from "./resources";

const phaseTitle: Record<Preparation["phase"], string> = {
  missing: "准备 Android 环境", downloading: "正在下载组件", verifying: "正在校验组件",
  installing: "正在安装组件", paused: "下载已暂停", failed: "组件准备失败", ready: "运行环境已就绪",
};

function ComponentRow(props: { item: Component }) {
  return <View style={{ paddingVertical: 10, borderTopWidth: 1, borderColor: colors().line, gap: 5 }}>
    <View style={row}>
      <Text style={{ flexGrow: 1, fontSize: 14, fontWeight: 600, color: colors().ink }}>{props.item.name}</Text>
      <Detail text={props.item.ready ? "已就绪" : "待准备"} />
    </View>
    <Detail text={props.item.version + " · " + size(props.item.sizeBytes)} />
    <Location path={props.item.location} />
    {props.item.issue ? <Detail text={props.item.issue} /> : null}
    <LicenseLink item={props.item} />
  </View>;
}

export function Components(props: { next?: () => void; skip?: () => void }) {
  const info = preparation;
  const imageTask = computed(() => info()?.images?.some(c => c.id === info()?.currentComponent));
  const phase = computed(() => {
    if (environmentReady()) return "ready";
    if (imageTask()) return info()?.components.some(c => c.downloadedBytes > 0 && !c.ready) ? "paused" : "missing";
    return info()?.phase ?? "missing";
  });
  const active = computed(() => !imageTask() && activeTransfer());
  const current = computed(() => info()?.components.find(c => c.id === info()?.currentComponent)?.name);
  const start = () => send({ kind: "prepareRuntime", acceptedLicenses: true }, "准备运行组件");
  return <View testID="components" style={{ gap: 12 }}>
    <Heading text={phaseTitle[phase()]} />
    <Detail text={"Android 实例共用以下运行组件。" + (props.skip && active() ? "退出引导后会继续下载。" : "")} />
    <View style={card()}>
      <Heading text="组件与许可" />
      <Each of={info()?.components} key={item => item.id}>{item => <ComponentRow item={item()} />}</Each>
      {active() || phase() === "paused" || phase() === "failed" ? <Progress id="component-progress"
        downloaded={info()?.downloadedBytes ?? 0} total={info()?.totalBytes ?? 0} /> : null}
      {current() && active() ? <Detail id="current-component" text={phaseTitle[phase()] + " · " + current()} /> : null}
      {!imageTask() && info()?.error ? <Text testID="preparation-error" role="alert" style={{ userSelect: "text", color: colors().bad }}>{info()?.error}</Text> : null}
      {imageTask() && activeTransfer() && !environmentReady() ? <Detail text="正在准备系统镜像，结束后可下载组件。" /> : null}
      {info()?.borrowed ? <Detail text="开发者指定的工具从原目录读取。更新后可重新检查。" /> : null}
      <View style={row}>
        {environmentReady() ? (props.next ? <Button id="guide-to-images" label="选择系统镜像" primary press={props.next} /> :
          <Button id="return-library" label="查看全部实例" press={() => navigate("instances")} />) : null}
        {!environmentReady() && !active() && info()?.components.some(c => !c.ready && !c.borrowed) ?
          <Button id="prepare-runtime" primary disabled={busy() || activeTransfer()} press={start}
            label={phase() === "paused" ? "同意并继续下载" : phase() === "failed" ? "同意并重试" : "同意并下载组件"} /> : null}
        {phase() === "downloading" ? <Button id="pause-preparation" label="暂停下载" disabled={busy()} press={() => send({ kind: "pausePreparation" }, "暂停下载")} /> : null}
        <Button id="recheck-runtime" label="重新检查" disabled={busy() || activeTransfer()} press={() => send({ kind: "recheckRuntime" }, "检查环境")} />
        {props.skip ? <Button id="guide-skip-components" label="跳过" press={props.skip} /> : null}
      </View>
    </View>
  </View>;
}

export function Environment() {
  return <View testID="environment" style={{ gap: 18 }}>
    <Title text="运行环境" />
    <View style={row}>
      <Button id="environment-components" label="运行组件" primary={environmentTab() === "components"} press={() => environmentTab.set("components")} />
      <Button id="environment-images" label="系统镜像" primary={environmentTab() === "images"} press={() => environmentTab.set("images")} />
    </View>
    {environmentTab() === "components" ? <Components /> : <ImageList />}
  </View>;
}
