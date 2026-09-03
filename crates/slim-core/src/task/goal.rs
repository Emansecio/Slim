#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GoalStatus {
    Active,
    Paused,
    Complete,
    Blocked,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Assurance {
    Verified,
    Unverified,
}

#[derive(Clone, Debug)]
pub struct Goal {
    budget: Option<u64>,
    used: u64,
    status: GoalStatus,
    assurance: Option<Assurance>,
}

impl Goal {
    pub fn new(budget: Option<u64>) -> Self {
        Self {
            budget,
            used: 0,
            status: GoalStatus::Active,
            assurance: None,
        }
    }

    pub fn consume(&mut self, amount: u64) -> Result<(), &'static str> {
        if self.status != GoalStatus::Active {
            return Err("goal is not active");
        }
        self.used = self.used.saturating_add(amount);
        if self.budget.is_some_and(|budget| self.used >= budget) {
            self.status = GoalStatus::Paused;
        }
        Ok(())
    }

    pub fn set_budget(&mut self, budget: Option<u64>) -> Result<(), &'static str> {
        if self.status != GoalStatus::Active || self.used != 0 {
            return Err("goal budget must be initialized first");
        }
        self.budget = budget;
        Ok(())
    }

    pub fn complete(&mut self, assurance: Assurance) -> Result<(), &'static str> {
        if self.status == GoalStatus::Blocked {
            return Err("blocked goal cannot complete");
        }
        self.status = GoalStatus::Complete;
        self.assurance = Some(assurance);
        Ok(())
    }

    pub fn status(&self) -> GoalStatus {
        self.status
    }

    pub fn assurance(&self) -> Option<Assurance> {
        self.assurance
    }
}
