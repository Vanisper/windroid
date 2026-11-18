//! 应用分组归属随实例保存。系统来源始终可以作为删除分组后的回退值。
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::GroupView;

#[derive(Default, Clone, Serialize, Deserialize)]
pub(super) struct Groups(BTreeMap<String, GroupSet>);

#[derive(Default, Clone, Serialize, Deserialize)]
struct GroupSet {
    names: BTreeMap<String, String>,
    membership: BTreeMap<String, String>,
    serial: u64,
}

impl Groups {
    pub fn load(home: &Path) -> io::Result<Self> {
        match fs::read(home.join("groups.json")) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }

    pub fn change(
        &mut self,
        home: Option<&Path>,
        instance: &str,
        kind: &str,
        data: &Value,
    ) -> Result<(), String> {
        let mut next = self.clone();
        let set = next.0.entry(instance.to_string()).or_default();
        let id = data["groupId"].as_str().unwrap_or_default();
        match kind {
            "createGroup" => {
                let name = data["name"].as_str().unwrap_or_default().trim();
                if name.is_empty()
                    || name.chars().count() > 40
                    || name.chars().any(char::is_control)
                {
                    return Err("分组名称需要 1 至 40 个字符".into());
                }
                if matches!(name, "已安装" | "系统应用") || set.names.values().any(|n| n == name)
                {
                    return Err("已存在同名分组".into());
                }
                set.serial += 1;
                set.names
                    .insert(format!("group-{}", set.serial), name.into());
            }
            "deleteGroup" => {
                if set.names.remove(id).is_none() {
                    return Err("请选择自建分组".into());
                }
                set.membership.retain(|_, v| v != id);
            }
            "moveApp" => {
                let package = data["package"]
                    .as_str()
                    .filter(|p| !p.is_empty())
                    .ok_or("请选择应用")?;
                if matches!(id, "" | "installed" | "system") {
                    set.membership.remove(package);
                } else if set.names.contains_key(id) {
                    set.membership.insert(package.into(), id.into());
                } else {
                    return Err("分组已不存在".into());
                }
            }
            _ => return Err("未知的分组操作".into()),
        }
        if let Some(home) = home {
            fs::create_dir_all(home).map_err(|e| e.to_string())?;
            let temp = home.join(format!(".groups-{}.tmp", std::process::id()));
            fs::write(&temp, serde_json::to_vec(&next).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
            fs::rename(&temp, home.join("groups.json")).map_err(|e| e.to_string())?;
        }
        *self = next;
        Ok(())
    }

    pub fn apply(&self, instance: &str, groups: &mut Vec<GroupView>) {
        let Some(set) = self.0.get(instance) else {
            return;
        };
        let mut custom: BTreeMap<_, _> = set
            .names
            .iter()
            .map(|(id, name)| {
                (
                    id,
                    GroupView {
                        id: id.clone(),
                        name: name.clone(),
                        apps: vec![],
                        count: 0,
                    },
                )
            })
            .collect();
        for source in groups.iter_mut() {
            for app in std::mem::take(&mut source.apps) {
                if let Some(group) = set
                    .membership
                    .get(&app.package)
                    .and_then(|id| custom.get_mut(id))
                {
                    group.apps.push(app);
                } else {
                    source.apps.push(app);
                }
            }
            source.count = source.apps.len();
        }
        for mut group in custom.into_values() {
            group
                .apps
                .sort_by_cached_key(|app| app.label.to_lowercase());
            group.count = group.apps.len();
            groups.push(group);
        }
    }
}
