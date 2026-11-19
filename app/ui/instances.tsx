import { computed, signal, View } from "nsh";
import { Input } from "nsh/base";
import { Menu } from "nsh/desktop";
import { Button, card, colors, Detail, Heading, row, Title } from "./controls";
import { ImageList, imageDescription } from "./images";
import { busy, creationImage, environmentReady, images, instances, navigate, report, screen, send, snapshot } from "./model";

const templates = [
  { id: "phone-1080", name: "手机", dimensions: "1080 × 2400" },
  { id: "tablet-1200", name: "平板", dimensions: "1920 × 1200" },
  { id: "tv-1080", name: "电视", dimensions: "1920 × 1080" },
  { id: "tv-4k", name: "4K 电视", dimensions: "3840 × 2160" },
  { id: "compact-720", name: "紧凑设备", dimensions: "1280 × 720" },
];

export function CreateInstance(props: { skip?: () => void }) {
  const startingImage = props.skip ? null : creationImage();
  const chosen = signal<string | null>(startingImage);
  const image = computed(() => images().find(i => i.ready && i.id === chosen()));
  const pick = signal(!startingImage);
  const select = (id: string) => { chosen.set(id); pick.set(false); };
  const cancel = props.skip ?? (() => navigate("instances"));
  const number = instances().length + 1;
  const name = signal(number === 1 ? "我的 Android" : "Android " + number);
  const template = signal(templates.find(item => item.id === snapshot()?.recommendedTemplate) ?? templates[0]);
  const invalid = computed(() => {
    const value = name().trim();
    return !value || Array.from(value).length > 64 || /[\u0000-\u001f\u007f-\u009f]/.test(value);
  });
  async function create() {
    const selectedImage = image();
    if (!selectedImage || invalid() || busy() || !environmentReady()) return;
    const origin = screen();
    if (await send({ kind: "createInstance", name: name().trim(), template: template().id, imageId: selectedImage.id }, "创建实例") && screen() === origin) navigate("library");
  }
  const chooseTemplate = () => Menu.popup(templates.map(t => ({ label: t.name + " · " + t.dimensions, checked: template().id === t.id, onSelect: () => template.set(t) })))
    .catch(e => report("无法显示设备模板 · " + String(e), true));
  return <View testID="creation" style={{ gap: 14 }}>
    <Title text={instances().length ? "新建实例" : "创建第一个实例"} />
    {pick() || !image() ? <View style={{ gap: 12 }}>
      <View style={row}>
        <Button id="cancel-image-selection" label={props.skip ? "跳过" : "取消"} press={cancel} />
        {image() ? <Button id="keep-image-selection" label="保留原选择" press={() => pick.set(false)} /> : null}
      </View>
      <ImageList select={select} />
    </View> : <View testID="create-form" style={{ gap: 18 }}>
        <Detail text="每个实例独立保存已安装的应用、账号和文件。" />
        <View style={{ ...card(), maxWidth: 580, gap: 14 }}>
          <Heading text="实例名称" />
          <Input testID="instance-name" accessibilityLabel="实例名称" value={name()} placeholder="例如：工作机"
            autoCorrect={false} onChange={e => name.set(e.value)} onSubmit={create}
            style={{ minHeight: 37, paddingHorizontal: 10, borderWidth: 1, borderRadius: 7, borderColor: colors().line, color: colors().ink, backgroundColor: colors().canvas }} />
          {invalid() ? <Detail text="请输入 1 至 64 个字符，名称不能包含控制字符。" /> : null}
          <Heading text="设备模板" />
          <Button id="choose-template" label={template().name + " · " + template().dimensions + "  ▾"} press={chooseTemplate} />
          <Detail text={"处理器 · 2 核" + (snapshot()?.templateRamMiB?.[template().id] ? "    内存 · " + snapshot()!.templateRamMiB![template().id] + " MiB" : "")} />
          <Detail text="用户空间 · 8 GiB" />
          <Heading text="系统镜像" />
          <Detail text={image() ? image()!.name + " · " + imageDescription(image()!) : "请选择镜像"} />
          <Button id="change-image" label="更换系统镜像" press={() => pick.set(true)} />
          <View style={row}>
            <Button id="create" label="创建实例" primary disabled={invalid() || busy() || !environmentReady()} press={create} />
            <Button id="cancel-create" label={props.skip ? "跳过" : "取消"} disabled={busy()} press={cancel} />
          </View>
        </View>
  </View>}</View>;
}
