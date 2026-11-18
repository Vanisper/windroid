import { computed, Scroll, signal, Text, View } from "nsh";
import { colors, Indicator, narrow } from "./controls";
import { currentId, environmentReady, instancePage, instances, instanceState, navigate, openInstance, screen, switching } from "./model";

const folded = signal<boolean | null>(null);
const compact = computed(() => folded() ?? narrow());

function NavItem(props: {
  id: string; label: string; glyph: string; active?: boolean; disabled?: boolean; press: () => void;
}) {
  return <View testID={props.id} role="button" accessibilityLabel={props.label} tooltip={props.label}
    accessibilityState={{ selected: !!props.active }} disabled={!!props.disabled} onPress={props.press}
    style={{ height: 34, flexDirection: "row", gap: 8, alignItems: "center",
      paddingHorizontal: compact() ? 0 : 8, justifyContent: compact() ? "center" : "flex-start",
      borderRadius: 6, opacity: props.disabled ? 0.5 : 1, backgroundColor: props.active ? colors().selected : colors().rail }}
    hoverStyle={{ backgroundColor: colors().wash }} focusVisibleStyle={{ backgroundColor: colors().selected }}>
    <Text style={{ width: 20, textAlign: "center", fontSize: 17, color: colors().ink }}>{props.glyph}</Text>
    {!compact() ? <Text style={{ fontSize: 13, fontWeight: 600, color: colors().ink }}>{props.label}</Text> : null}
  </View>;
}

export function Sidebar() {
  return <View testID="sidebar" style={{ width: compact() ? 54 : 174, padding: 8, gap: 5, minHeight: 0,
    backgroundColor: colors().rail, borderRightWidth: 1, borderColor: colors().line }}>
    <NavItem id="toggle-sidebar" label={compact() ? "展开侧栏" : "折叠侧栏"} glyph={compact() ? "»" : "«"} press={() => folded.set(!compact())} />
    <NavItem id="all-instances" label="实例" glyph="▦" active={screen() === "instances"} press={() => navigate("instances")} />
    <NavItem id="new-instance" label="新建实例" glyph="＋" active={screen() === "create"}
      disabled={!environmentReady() || switching()} press={() => navigate("create")} />
    <Scroll testID="instance-navigation" style={{ flex: 1, minHeight: 0 }} contentStyle={{ gap: 2 }}>
      <Each of={instances()} key={item => item.id}>{item =>
        <View testID={"instance-" + item().id} role="button" accessibilityLabel={item().name + " · " + (item().summary ?? instanceState(item()))}
          tooltip={item().name + " · " + (item().summary ?? instanceState(item()))}
          accessibilityState={{ selected: instancePage() && currentId() === item().id }} disabled={switching()}
          onPress={() => void openInstance(item().id, "library")}
          style={{ height: 45, paddingHorizontal: compact() ? 3 : 8, gap: 5, borderRadius: 6, minWidth: 0,
            flexDirection: "row", alignItems: "center", justifyContent: compact() ? "center" : "flex-start",
            backgroundColor: instancePage() && currentId() === item().id ? colors().selected : colors().rail }}
          hoverStyle={{ backgroundColor: colors().wash }} focusVisibleStyle={{ backgroundColor: colors().selected }}>
          <Indicator state={item().state} />
          <View style={{ minWidth: 0, flexShrink: 1, gap: 2 }}>
            <Text style={{ fontSize: 13, numberOfLines: 1, fontWeight: 600, color: colors().ink }}>
              {compact() ? Array.from(item().name)[0] : item().name}
            </Text>
            {!compact() ? <Text style={{ fontSize: 11, numberOfLines: 1, color: colors().muted }}>
              {item().state === "starting" || item().state === "stalled" ? item().summary ?? instanceState(item()) : instanceState(item())}
            </Text> : null}
          </View>
        </View>
      }</Each>
    </Scroll>
    <NavItem id="show-environment" label="运行环境" glyph="▤" active={screen() === "environment"} press={() => navigate("environment")} />
    <NavItem id="show-settings" label="设置" glyph="⚙" active={screen() === "settings" || screen() === "guide"} press={() => navigate("settings")} />
  </View>;
}
