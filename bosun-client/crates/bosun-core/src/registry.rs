use std::collections::{HashMap, VecDeque};

use crate::resource::{Resource, ResourceId};

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RegistryError {
    #[error("duplicate resource id: {0}")]
    DuplicateId(ResourceId),
    #[error("unknown handle referenced: {0}")]
    UnknownHandle(ResourceId),
    #[error("dependency cycle detected; stuck nodes: {nodes}")]
    Cycle { nodes: String },
}

#[derive(Debug, Default)]
pub struct Registry {
    resources: Vec<Resource>,
    by_id: HashMap<ResourceId, usize>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, r: Resource) -> Result<ResourceId, RegistryError> {
        if self.by_id.contains_key(&r.id) {
            return Err(RegistryError::DuplicateId(r.id.clone()));
        }
        let id = r.id.clone();
        self.by_id.insert(id.clone(), self.resources.len());
        self.resources.push(r);
        Ok(id)
    }

    pub fn get(&self, id: &ResourceId) -> Option<&Resource> {
        self.by_id.get(id).map(|&i| &self.resources[i])
    }

    pub fn all(&self) -> &[Resource] {
        &self.resources
    }

    /// Топологический порядок ресурсов. При обнаружении цикла возвращает
    /// `Cycle { nodes }` — отсортированный список вершин, у которых остался
    /// ненулевой in-degree (Kahn не смог их «погасить»). Это множество
    /// застрявших вершин, а не конкретный path по циклу: восстановить путь
    /// требует back-edge DFS, который для MVP избыточен. Если нужен exact
    /// cycle, выводить его придётся в Phase 5+ при отладке сложных манифестов.
    ///
    /// В качестве рёбер используются `depends_on`, `reload_on` и `restart_on`:
    /// первое выражает «применяй меня после X», два других — notify-семантику.
    /// Для порядка применения они эквивалентны: notify-источник должен быть
    /// планирован до подписчика, чтобы подписчик мог узнать о его изменении.
    pub fn topological_order(&self) -> Result<Vec<ResourceId>, RegistryError> {
        // Kahn algorithm: рёбра — от dependency к dependent.
        let n = self.resources.len();
        let mut in_degree: HashMap<&ResourceId, usize> =
            self.resources.iter().map(|r| (&r.id, 0)).collect();
        let mut adj: HashMap<&ResourceId, Vec<&ResourceId>> = HashMap::new();

        for r in &self.resources {
            // Все три источника связей формируют идентичные рёбра в графе
            // порядка применения. Дубль одного и того же id внутри одного
            // ресурса трактуется как несколько рёбер — Kahn это переносит:
            // in_degree корректно убывает на каждое pop'нутое sucessor-звено.
            for dep in r
                .depends_on
                .iter()
                .chain(r.reload_on.iter())
                .chain(r.restart_on.iter())
            {
                if !self.by_id.contains_key(dep) {
                    return Err(RegistryError::UnknownHandle(dep.clone()));
                }
                adj.entry(dep).or_default().push(&r.id);
                *in_degree.entry(&r.id).or_insert(0) += 1;
            }
        }

        let mut queue: VecDeque<&ResourceId> = in_degree
            .iter()
            .filter(|(_, &d)| d == 0)
            .map(|(k, _)| *k)
            .collect();
        let mut order: Vec<ResourceId> = Vec::with_capacity(n);

        while let Some(id) = queue.pop_front() {
            order.push(id.clone());
            if let Some(successors) = adj.get(id) {
                for s in successors {
                    if let Some(d) = in_degree.get_mut(*s) {
                        *d -= 1;
                        if *d == 0 {
                            queue.push_back(s);
                        }
                    }
                }
            }
        }

        if order.len() != n {
            // Цикл. HashMap-итерация не детерминирована, поэтому сортируем
            // stuck-вершины перед формированием сообщения, иначе один и тот
            // же манифест давал бы разный текст ошибки между запусками.
            let mut stuck: Vec<String> = in_degree
                .iter()
                .filter(|(_, &d)| d > 0)
                .map(|(k, _)| k.to_string())
                .collect();
            stuck.sort();
            return Err(RegistryError::Cycle {
                nodes: stuck.join(", "),
            });
        }
        Ok(order)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::resource::ResourceKind;

    fn kind(s: &str) -> ResourceKind {
        ResourceKind::try_new(s).unwrap()
    }

    fn res(kind_str: &str, name: &str, deps: Vec<ResourceId>) -> Resource {
        let k = kind(kind_str);
        let id = ResourceId::new(&k, name);
        Resource {
            id,
            kind: k,
            spec_version: 1,
            payload: serde_json::json!({}),
            reload_on: Vec::new(),
            restart_on: Vec::new(),
            depends_on: deps,
        }
    }

    #[test]
    fn add_returns_id() {
        let mut reg = Registry::new();
        let id = reg.add(res("apt.package", "nginx", vec![])).unwrap();
        assert_eq!(id.as_str(), "apt.package:nginx");
        assert!(reg.get(&id).is_some());
    }

    #[test]
    fn duplicate_id_rejected() {
        let mut reg = Registry::new();
        reg.add(res("apt.package", "nginx", vec![])).unwrap();
        let err = reg.add(res("apt.package", "nginx", vec![])).unwrap_err();
        assert!(matches!(err, RegistryError::DuplicateId(_)));
    }

    #[test]
    fn topo_order_independent_resources() {
        let mut reg = Registry::new();
        reg.add(res("apt.package", "a", vec![])).unwrap();
        reg.add(res("apt.package", "b", vec![])).unwrap();
        let order = reg.topological_order().unwrap();
        assert_eq!(order.len(), 2);
    }

    #[test]
    fn topo_order_respects_depends_on() {
        let mut reg = Registry::new();
        let a = reg.add(res("apt.package", "a", vec![])).unwrap();
        reg.add(res("file.content", "/b", vec![a.clone()])).unwrap();
        let order = reg.topological_order().unwrap();
        assert_eq!(order[0], a);
    }

    #[test]
    fn cycle_detected() {
        let mut reg = Registry::new();
        // Сначала создаём оба, потом добавим связь — но связь хранится в depends_on.
        // Создадим вручную с обратными ссылками.
        let ka = kind("apt.package");
        let id_a = ResourceId::new(&ka, "a");
        let id_b = ResourceId::new(&ka, "b");
        reg.add(Resource {
            id: id_a.clone(),
            kind: ka.clone(),
            spec_version: 1,
            payload: serde_json::json!({}),
            reload_on: vec![],
            restart_on: vec![],
            depends_on: vec![id_b.clone()],
        })
        .unwrap();
        reg.add(Resource {
            id: id_b.clone(),
            kind: ka,
            spec_version: 1,
            payload: serde_json::json!({}),
            reload_on: vec![],
            restart_on: vec![],
            depends_on: vec![id_a.clone()],
        })
        .unwrap();
        let err = reg.topological_order().unwrap_err();
        let nodes = match &err {
            RegistryError::Cycle { nodes } => nodes,
            other => unreachable!("expected Cycle, got {other:?}"),
        };
        // Сортировка детерминирована: "apt.package:a" < "apt.package:b".
        assert_eq!(nodes, "apt.package:a, apt.package:b");
        let msg = err.to_string();
        assert!(msg.contains("apt.package:a"), "msg={msg}");
        assert!(msg.contains("apt.package:b"), "msg={msg}");
    }

    #[test]
    fn unknown_handle_rejected() {
        let mut reg = Registry::new();
        let ghost = ResourceId::new(&kind("apt.package"), "ghost");
        reg.add(res("file.content", "/a", vec![ghost])).unwrap();
        let err = reg.topological_order().unwrap_err();
        assert!(matches!(err, RegistryError::UnknownHandle(_)));
    }

    #[test]
    fn topo_order_respects_restart_on() {
        // restart_on создаёт такое же отношение «применить раньше», как
        // depends_on/reload_on: notify-источник должен быть впереди
        // notify-подписчика в порядке apply.
        let mut reg = Registry::new();
        let cfg = reg.add(res("file.content", "/etc/cfg", vec![])).unwrap();
        let kind_svc = kind("runr.service");
        let id_svc = ResourceId::new(&kind_svc, "svc");
        reg.add(Resource {
            id: id_svc.clone(),
            kind: kind_svc,
            spec_version: 1,
            payload: serde_json::json!({}),
            reload_on: vec![],
            restart_on: vec![cfg.clone()],
            depends_on: vec![],
        })
        .unwrap();
        let order = reg.topological_order().unwrap();
        // cfg должен идти раньше svc.
        let pos = |id: &ResourceId| order.iter().position(|x| x == id).unwrap();
        assert!(
            pos(&cfg) < pos(&id_svc),
            "cfg should precede svc in topo order, got {order:?}"
        );
    }

    #[test]
    fn restart_on_unknown_handle_rejected() {
        let mut reg = Registry::new();
        let ghost = ResourceId::new(&kind("file.content"), "/ghost");
        let kind_svc = kind("runr.service");
        let id_svc = ResourceId::new(&kind_svc, "svc");
        reg.add(Resource {
            id: id_svc,
            kind: kind_svc,
            spec_version: 1,
            payload: serde_json::json!({}),
            reload_on: vec![],
            restart_on: vec![ghost],
            depends_on: vec![],
        })
        .unwrap();
        let err = reg.topological_order().unwrap_err();
        assert!(matches!(err, RegistryError::UnknownHandle(_)));
    }
}
