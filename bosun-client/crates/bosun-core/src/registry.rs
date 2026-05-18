use std::collections::{HashMap, VecDeque};

use crate::resource::{Resource, ResourceId};

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RegistryError {
    #[error("duplicate resource id: {0}")]
    DuplicateId(ResourceId),
    #[error("unknown handle referenced: {0}")]
    UnknownHandle(ResourceId),
    #[error("dependency cycle detected: {path}")]
    Cycle { path: String },
}

#[derive(Default)]
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

    pub fn topological_order(&self) -> Result<Vec<ResourceId>, RegistryError> {
        // Kahn algorithm: рёбра — от dependency к dependent.
        // reload_on и depends_on в MVP трактуются одинаково.
        let n = self.resources.len();
        let mut in_degree: HashMap<&ResourceId, usize> =
            self.resources.iter().map(|r| (&r.id, 0)).collect();
        let mut adj: HashMap<&ResourceId, Vec<&ResourceId>> = HashMap::new();

        for r in &self.resources {
            for dep in r.depends_on.iter().chain(r.reload_on.iter()) {
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
            // Цикл. Собираем хоть какую-то цепочку для сообщения.
            let stuck: Vec<String> = in_degree
                .iter()
                .filter(|(_, &d)| d > 0)
                .map(|(k, _)| k.to_string())
                .collect();
            return Err(RegistryError::Cycle {
                path: stuck.join(" -> "),
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

    // Helper since from_static needs &'static str, but tests use dynamic strings.
    impl ResourceKind {
        fn from_static_to_owned(s: &str) -> Self {
            // Test-only: проксируем через try_new (валидные kind'ы в тестах).
            Self::try_new(s).unwrap()
        }
    }

    fn res(kind: &str, name: &str, deps: Vec<ResourceId>) -> Resource {
        let k = ResourceKind::from_static_to_owned(kind);
        let id = ResourceId::new(&k, name);
        Resource {
            id,
            kind: k,
            spec_version: 1,
            payload: serde_json::json!({}),
            reload_on: Vec::new(),
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
        let ka = ResourceKind::from_static_to_owned("apt.package");
        let id_a = ResourceId::new(&ka, "a");
        let id_b = ResourceId::new(&ka, "b");
        reg.add(Resource {
            id: id_a.clone(),
            kind: ka.clone(),
            spec_version: 1,
            payload: serde_json::json!({}),
            reload_on: vec![],
            depends_on: vec![id_b.clone()],
        })
        .unwrap();
        reg.add(Resource {
            id: id_b.clone(),
            kind: ka,
            spec_version: 1,
            payload: serde_json::json!({}),
            reload_on: vec![],
            depends_on: vec![id_a.clone()],
        })
        .unwrap();
        let err = reg.topological_order().unwrap_err();
        assert!(matches!(err, RegistryError::Cycle { .. }));
    }

    #[test]
    fn unknown_handle_rejected() {
        let mut reg = Registry::new();
        let ghost = ResourceId::new(&ResourceKind::try_new("apt.package").unwrap(), "ghost");
        reg.add(res("file.content", "/a", vec![ghost])).unwrap();
        let err = reg.topological_order().unwrap_err();
        assert!(matches!(err, RegistryError::UnknownHandle(_)));
    }
}
