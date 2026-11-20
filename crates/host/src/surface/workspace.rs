use std::cell::RefCell;
use std::path::{Path, PathBuf};

use nsh_platform_macos::objc2::rc::Retained;
use nsh_platform_macos::objc2::runtime::{AnyObject, ProtocolObject};
use nsh_platform_macos::objc2::{
    DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
};
use nsh_platform_macos::objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSModalResponseOK, NSOpenSavePanelDelegate,
};
use nsh_platform_macos::objc2_foundation::{
    NSCocoaErrorDomain, NSDictionary, NSError, NSLocalizedDescriptionKey, NSObject,
    NSObjectProtocol, NSString, NSURL, NSUserCancelledError,
};
use runtime::workspace::{self, Workspace};

fn prompt(mtm: MainThreadMarker, title: &str, detail: &str, action: &str, cancel: &str) -> bool {
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str(title));
    alert.setInformativeText(&NSString::from_str(detail));
    alert.addButtonWithTitle(&NSString::from_str(action));
    alert.addButtonWithTitle(&NSString::from_str(cancel));
    alert.runModal() == NSAlertFirstButtonReturn
}

pub fn review_workspace(mtm: MainThreadMarker, path: &Path) -> Result<Option<Workspace>, String> {
    let state = workspace::inspect(path).map_err(|e| e.to_string())?;
    if let Workspace::Resources(names) = &state {
        let detail = format!(
            "{}\n\n已核验下列预放资源：\n{}",
            path.display(),
            names.join("\n")
        );
        if !prompt(
            mtm,
            "使用此目录新建工作区？",
            &detail,
            "在此新建工作区",
            "取消",
        ) {
            return Ok(None);
        }
    }
    Ok(Some(state))
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = RefCell<Option<(PathBuf, Workspace)>>]
    #[name = "WindroidWorkspacePickerDelegate"]
    struct WorkspacePickerDelegate;
    unsafe impl NSObjectProtocol for WorkspacePickerDelegate {}
    unsafe impl NSOpenSavePanelDelegate for WorkspacePickerDelegate {
        // NSOpenPanel 在成功返回 URL 时可以给所选目录写入系统授权属性。
        // 在系统接受路径之前检查工作区和预放资源的建库意愿，使取消、拒绝都保持只读。
        // https://developer.apple.com/documentation/appkit/nsopensavepaneldelegate/panel(_:validate:)
        #[unsafe(method(panel:validateURL:error:))]
        fn validate(&self, _panel: &AnyObject, url: &NSURL, error: Option<&mut *mut NSError>) -> bool {
            self.ivars().take();
            let result = url.to_file_path().ok_or_else(|| "请选择本机目录".to_owned()).and_then(|path| {
                review_workspace(self.mtm(), &path)
                    .map_err(|reason| format!("{}\n\n{reason}", path.display()))
                    .map(|state| state.map(|s| (path, s)))
            });
            match result {
                Ok(Some(selection)) => {
                    self.ivars().replace(Some(selection));
                    true
                }
                stopped => {
                    if let Some(out) = error {
                        *out = Retained::autorelease_ptr(picker_error(stopped.err()));
                    }
                    false
                }
            }
        }
    }
);

fn picker_error(reason: Option<String>) -> Retained<NSError> {
    let Some(reason) = reason else {
        return unsafe {
            NSError::errorWithDomain_code_userInfo(NSCocoaErrorDomain, NSUserCancelledError, None)
        };
    };
    let message = NSString::from_str(&reason);
    let info = NSDictionary::from_slices(
        &[unsafe { NSLocalizedDescriptionKey }],
        &[&*message as &AnyObject],
    );
    // NSError 接受本地化说明的 NSString，并在返回的错误对象中保留字典。
    unsafe {
        NSError::errorWithDomain_code_userInfo(
            &NSString::from_str("Windroid.Workspace"),
            1,
            Some(&info),
        )
    }
}

pub fn choose_another_workspace(
    mtm: MainThreadMarker,
    home: &Path,
    error: &str,
) -> Option<(PathBuf, Workspace)> {
    let detail = format!("{}\n\n{error}", home.display());
    while prompt(mtm, "无法打开此工作区", &detail, "选择其他目录…", "退出") {
        let panel = super::file_panel(mtm, "选择工作区目录", true);
        panel.setPrompt(Some(&NSString::from_str("选择")));
        if let Some(parent) = home
            .ancestors()
            .skip(1)
            .find(|p| p.is_absolute() && p.is_dir())
            && let Some(url) = NSURL::from_directory_path(parent)
        {
            panel.setDirectoryURL(Some(&url));
        }
        let delegate = mtm
            .alloc::<WorkspacePickerDelegate>()
            .set_ivars(RefCell::new(None));
        let delegate: Retained<WorkspacePickerDelegate> =
            unsafe { msg_send![super(delegate), init] };
        // AppKit 弱引用 delegate，本地持有至模态面板结束。
        unsafe { panel.setDelegate(Some(ProtocolObject::from_ref(&*delegate))) };
        let response = panel.runModal();
        unsafe { panel.setDelegate(None) };
        if response == NSModalResponseOK
            && let Some(selection) = delegate.ivars().take()
        {
            return Some(selection);
        }
    }
    None
}
