import { signal, View } from "nsh";
import { Button, card, Detail, row, Title } from "./controls";
import { CreateInstance } from "./instances";
import { environmentReady, guideOrigin, navigate } from "./model";
import { Components } from "./preparation";

export function Guide() {
  const step = signal<"welcome" | "components" | "images">("welcome");
  const fromSettings = guideOrigin() === "settings";
  const finish = () => navigate(fromSettings ? "settings" : "instances");
  return <View testID="guide" style={{ gap: 16 }}>
    {step() === "welcome" ? <View style={{ ...card(), padding: 24, gap: 16 }}>
      <Title text={fromSettings ? "使用引导" : "欢迎使用 Windroid"} />
      <Detail text="准备运行组件，选择系统镜像，然后创建 Android 实例。每步都可以跳过。" />
      <View style={row}>
        <Button id="guide-begin" label="开始设置" primary press={() => step.set(environmentReady() ? "images" : "components")} />
        <Button id="guide-back" label={fromSettings ? "返回设置" : "直接进入"} press={finish} />
      </View>
    </View> : step() === "components" ? <View style={{ gap: 14 }}>
      <Title text="运行组件" />
      <Components next={() => step.set("images")} skip={finish} />
    </View> : <CreateInstance skip={finish} />}
  </View>;
}
