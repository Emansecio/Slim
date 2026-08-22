use std::collections::HashSet;

#[derive(Debug, Default)]
pub struct Plan {
    nodes: Vec<PlanNode>,
    version: u64,
    approved: bool,
    completed: HashSet<String>,
}

#[derive(Debug)]
struct PlanNode {
    id: String,
    dependencies: Vec<String>,
}

impl Plan {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_node(&mut self, id: &str, dependencies: &[&str]) -> Result<(), &'static str> {
        if self.nodes.iter().any(|node| node.id == id) {
            return Err("duplicate plan node");
        }
        if dependencies.iter().any(|dependency| dependency == &id) {
            return Err("plan node cannot depend on itself");
        }
        if dependencies
            .iter()
            .any(|dependency| !self.nodes.iter().any(|node| node.id == *dependency))
        {
            return Err("plan dependency not found");
        }
        self.nodes.push(PlanNode {
            id: id.into(),
            dependencies: dependencies
                .iter()
                .map(|dependency| (*dependency).into())
                .collect(),
        });
        self.approved = false;
        Ok(())
    }

    pub fn ready_nodes(&self) -> Vec<String> {
        self.nodes
            .iter()
            .filter(|node| {
                !self.completed.contains(&node.id)
                    && node.dependencies.iter().all(|dependency| {
                        self.completed.contains(dependency)
                            || !self
                                .nodes
                                .iter()
                                .any(|candidate| candidate.id == *dependency)
                    })
            })
            .map(|node| node.id.clone())
            .collect()
    }

    pub fn approve(&mut self) -> Result<u64, &'static str> {
        if self.nodes.is_empty() {
            return Err("cannot approve empty plan");
        }
        self.version += 1;
        self.approved = true;
        Ok(self.version)
    }

    pub fn is_approved(&self) -> bool {
        self.approved
    }
}
