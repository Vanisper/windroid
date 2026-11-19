//! 每个实例至多持有一个非模态 APK 面板。完成回调沿用创建面板时绑定的实例。

use std::collections::{BTreeMap, btree_map::Entry};
use std::path::PathBuf;

use host::surface::WindowEvent;

pub(super) struct ApkPickers<Panel> {
    panels: BTreeMap<String, Panel>,
}

impl<Panel> ApkPickers<Panel> {
    pub fn new() -> Self {
        Self {
            panels: BTreeMap::new(),
        }
    }

    pub fn show(
        &mut self,
        instance_id: String,
        create: impl FnOnce(String) -> Panel,
        focus: impl FnOnce(&Panel),
    ) {
        match self.panels.entry(instance_id) {
            Entry::Occupied(panel) => focus(panel.get()),
            Entry::Vacant(slot) => {
                let panel = create(slot.key().clone());
                slot.insert(panel);
            }
        }
    }

    pub fn finish(&mut self, instance_id: String, path: Option<PathBuf>) -> Option<WindowEvent> {
        self.panels.remove(&instance_id)?;
        path.map(|path| WindowEvent::ApkPicked { instance_id, path })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 复用 AppKit 面板的注册与完成路径。只把 NSOpenPanel 句柄换成可辨别的标记，
    // 测试端持有每个 create 捕获的目标，按用户确认或取消的顺序交回。
    fn open(pickers: &mut ApkPickers<u8>, instance: &str, handle: u8) -> String {
        let mut callback_target = None;
        pickers.show(
            instance.into(),
            |target| {
                callback_target = Some(target);
                handle
            },
            |_| panic!("应该新建面板"),
        );
        callback_target.unwrap()
    }

    fn focus(pickers: &mut ApkPickers<u8>, instance: &str, handle: u8) {
        let mut focused = None;
        pickers.show(
            instance.into(),
            |_| panic!("重复打开应该聚焦已有面板"),
            |panel| focused = Some(*panel),
        );
        assert_eq!(focused, Some(handle));
    }

    fn confirm(pickers: &mut ApkPickers<u8>, target: String, instance: &str, file: &str) {
        let Some(WindowEvent::ApkPicked { instance_id, path }) =
            pickers.finish(target, Some(file.into()))
        else {
            panic!("应向宿主回报 APK 选择");
        };
        assert_eq!(instance_id, instance);
        assert_eq!(path, PathBuf::from(file));
    }

    #[test]
    fn confirming_a_after_opening_b_preserves_b_and_reopens_only_a() {
        let mut pickers = ApkPickers::new();
        let a = open(&mut pickers, "a", 1);
        let b = open(&mut pickers, "b", 2);
        focus(&mut pickers, "a", 1);
        focus(&mut pickers, "b", 2);
        confirm(&mut pickers, a, "a", "/one.apk");
        let new_a = open(&mut pickers, "a", 3);
        focus(&mut pickers, "b", 2);
        confirm(&mut pickers, b, "b", "/two.apk");
        focus(&mut pickers, "a", 3);
        assert!(pickers.finish(new_a, None).is_none());
        assert!(pickers.panels.is_empty());
    }

    #[test]
    fn cancelling_b_then_reopening_it_keeps_a_bound_to_its_original_callback() {
        let mut pickers = ApkPickers::new();
        let a = open(&mut pickers, "a", 1);
        let b = open(&mut pickers, "b", 2);
        assert!(pickers.finish(b, None).is_none());
        focus(&mut pickers, "a", 1);
        let new_b = open(&mut pickers, "b", 3);
        confirm(&mut pickers, new_b, "b", "/two.apk");
        confirm(&mut pickers, a, "a", "/one.apk");
        assert!(pickers.panels.is_empty());
    }
}
