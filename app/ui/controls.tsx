import { colorScheme, computed, effect, signal, Text, View, windowSize } from "nsh";
import { Dialog } from "nsh/base";
import { currentId, type DeviceState } from "./model";

const light = {
  canvas: "#FBFCFA", rail: "#F1F4F1", card: "#FFFFFF", ink: "#192922", muted: "#53685C", faint: "#67776D",
  line: "#D7DFD8", wash: "#E7EDE8", accent: "#186749", selected: "#DCEEE2", onAccent: "#FFFFFF",
  warn: "#82560B", warnBg: "#FBF2D9", bad: "#A83830", badBg: "#FAE9E5",
};
const dark: typeof light = {
  canvas: "#18211C", rail: "#111914", card: "#1F2A23", ink: "#E8F0E9", muted: "#B4C3B7", faint: "#95A69A",
  line: "#38483C", wash: "#2B382F", accent: "#8BD7AE", selected: "#284335", onAccent: "#10271A",
  warn: "#E9C26B", warnBg: "#3A3020", bad: "#FFABA2", badBg: "#452A26",
};
export const colors = computed(() => colorScheme() === "dark" ? dark : light);
export const narrow = computed(() => windowSize().width < 660);
export const row = { flexDirection: "row", flexWrap: "wrap", alignItems: "center", gap: 8 } as const;
export const card = () => ({ backgroundColor: colors().card, borderWidth: 1, borderColor: colors().line, borderRadius: 12, padding: 16, gap: 10 });

export function Title(props: { text: string }) {
  return <Text role="header" accessibilityLevel={1} style={{ fontSize: narrow() ? 24 : 30, fontWeight: 650, color: colors().ink }}>{props.text}</Text>;
}
export function Heading(props: { text: string }) {
  return <Text role="header" accessibilityLevel={2} style={{ fontSize: 16, fontWeight: 650, color: colors().ink }}>{props.text}</Text>;
}
export function Detail(props: { text: string; id?: string }) {
  return <Text testID={props.id} style={{ fontSize: 13, color: colors().muted }}>{props.text}</Text>;
}
export function Button(props: {
  id: string; label: string; press: () => void; disabled?: boolean; primary?: boolean; danger?: boolean; tooltip?: string;
}) {
  return (
    <View testID={props.id} role="button" accessibilityLabel={props.label} tooltip={props.tooltip}
      disabled={props.disabled === true} onPress={props.press}
      style={{ alignItems: "center", justifyContent: "center", alignSelf: "flex-start", minHeight: 32,
        paddingHorizontal: 12, paddingVertical: 6, borderRadius: 8, borderWidth: 1,
        borderColor: props.primary ? colors().accent : colors().line,
        backgroundColor: props.primary ? colors().accent : colors().card,
        opacity: props.disabled ? 0.5 : 1 }}
      hoverStyle={{ opacity: 0.83 }} activeStyle={{ opacity: 0.66 }} focusVisibleStyle={{ borderColor: colors().accent }}>
      <Text style={{ fontSize: 13, fontWeight: 600, color: props.primary ? colors().onAccent : props.danger ? colors().bad : colors().ink }}>{props.label}</Text>
    </View>
  );
}

export function Indicator(props: { state: DeviceState }) {
  const color = computed(() => {
    switch (props.state) {
      case "ready": case "streaming": return colors().accent;
      case "starting": case "stopping": case "stalled": return colors().warn;
      case "failed": return colors().bad;
      default: return colors().faint;
    }
  });
  return <View style={{ width: 8, height: 8, borderRadius: 4, backgroundColor: color() }} />;
}

interface Confirmation { id: string; title: string; detail: string; label: string; action: () => void }
export const confirmation = signal<Confirmation | null>(null);
export function ConfirmDialog() {
  const request = computed(() => {
    const value = confirmation();
    return value?.id === currentId() ? value : null;
  });
  const close = () => confirmation.set(null);
  effect(() => { if (confirmation() && confirmation()?.id !== currentId()) close(); });
  return <Dialog open={request() !== null} animation="fade" role="alertdialog" accessibilityLabel={request()?.title ?? "确认操作"}
    onCancel={close} backdropStyle={{ backgroundColor: "#0A170F70" }}
    style={{ ...card(), width: Math.min(380, Math.max(200, windowSize().width - 32)), padding: 20, gap: 14 }}>
    <Heading text={request()?.title ?? ""} /><Detail text={request()?.detail ?? ""} />
    <View style={row}>
      <Button id="cancel-confirmation" label="取消" press={close} />
      <Button id="confirm-action" label={request()?.label ?? "确定"} danger press={() => { const r = request(); close(); r?.action(); }} />
    </View>
  </Dialog>;
}
