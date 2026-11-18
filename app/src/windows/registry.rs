//! 按实例和包路由窗口。窗口退场时保留其键，保证旧显示已回收再处理恢复。

use host::surface::WindowKey;
use std::collections::BTreeMap;

use super::Ticket;

pub(super) const MAX_APP_WINDOWS: usize = 8;

#[derive(Debug, Eq, PartialEq)]
pub(super) enum Route {
    Focus,
    AfterCleanup,
    Create,
    AtCapacity,
}

pub(super) struct Retiring<Worker, Request> {
    generation: u64,
    pub worker: Worker,
    pub reopen: Option<Request>,
}

pub(super) struct Registry<Entry, Worker, Request> {
    pub live: BTreeMap<WindowKey, Entry>,
    pub retiring: BTreeMap<WindowKey, Retiring<Worker, Request>>,
}

impl<Entry, Worker, Request> Registry<Entry, Worker, Request> {
    pub fn new() -> Self {
        Self {
            live: BTreeMap::new(),
            retiring: BTreeMap::new(),
        }
    }

    pub fn route(&self, key: &WindowKey) -> Route {
        if self.live.contains_key(key) {
            return Route::Focus;
        }
        if self.retiring.contains_key(key) {
            return Route::AfterCleanup;
        }
        if key.package.is_some() {
            let used = self
                .live
                .keys()
                .chain(self.retiring.keys())
                .filter(|key| key.package.is_some())
                .count();
            if used >= MAX_APP_WINDOWS {
                return Route::AtCapacity;
            }
        }
        Route::Create
    }

    pub fn retire(&mut self, key: WindowKey, generation: u64, worker: Worker) {
        self.retiring.insert(
            key,
            Retiring {
                generation,
                worker,
                reopen: None,
            },
        );
    }

    pub fn finished(&mut self, ticket: &Ticket) -> Option<Retiring<Worker, Request>> {
        self.retiring
            .get(&ticket.key)
            .filter(|old| old.generation == ticket.generation)?;
        self.retiring.remove(&ticket.key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(id: &str, package: Option<&str>) -> WindowKey {
        WindowKey {
            instance_id: id.into(),
            package: package.map(str::to_owned),
        }
    }

    #[test]
    fn duplicate_opens_focus_only_their_own_instance_and_package() {
        let mut r: Registry<u8, (), ()> = Registry::new();
        let a = key("alpha", Some("org.example"));
        let b = key("beta", Some("org.example"));
        let device = key("alpha", None);
        assert_eq!(r.route(&a), Route::Create);
        r.live.insert(a.clone(), 3);
        assert_eq!(r.route(&a), Route::Focus);
        assert_eq!(r.route(&b), Route::Create);
        assert_eq!(r.route(&device), Route::Create);
        r.live.insert(device.clone(), 0);
        assert_eq!(r.route(&device), Route::Focus);
    }

    #[test]
    fn old_display_cleanup_finishes_before_reopening_and_stale_end_is_ignored() {
        let mut r: Registry<(), (), &str> = Registry::new();
        let app = key("alpha", Some("org.example"));
        r.retire(app.clone(), 2, ());
        assert_eq!(r.route(&app), Route::AfterCleanup);
        r.retiring.get_mut(&app).unwrap().reopen = Some("focus");
        assert!(
            r.finished(&Ticket {
                key: app.clone(),
                generation: 1
            })
            .is_none()
        );
        assert_eq!(r.route(&app), Route::AfterCleanup);
        assert_eq!(
            r.finished(&Ticket {
                key: app.clone(),
                generation: 2
            })
            .unwrap()
            .reopen,
            Some("focus")
        );
        assert_eq!(r.route(&app), Route::Create);
    }

    #[test]
    fn cap_counts_active_and_retiring_apps_but_not_device_windows() {
        let mut r: Registry<(), (), ()> = Registry::new();
        for i in 0..MAX_APP_WINDOWS - 1 {
            r.live.insert(key("alpha", Some(&format!("app.{i}"))), ());
        }
        r.retire(key("beta", Some("retiring")), 3, ());
        assert_eq!(r.route(&key("beta", Some("next"))), Route::AtCapacity);
        assert_eq!(r.route(&key("beta", None)), Route::Create);
        assert_eq!(r.route(&key("alpha", Some("app.0"))), Route::Focus);
    }
}
