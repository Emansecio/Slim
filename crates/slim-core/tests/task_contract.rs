use slim_core::task::{Assurance, Goal, GoalStatus, Plan, TodoStatus, TodoTracker};

#[test]
fn todo_allows_one_in_progress_item_per_agent() {
    let mut todo = TodoTracker::new();
    let first = todo.add("first");
    let second = todo.add("second");
    todo.set_status(first, TodoStatus::InProgress)
        .expect("start");
    assert!(todo.set_status(second, TodoStatus::InProgress).is_err());
    todo.set_status(first, TodoStatus::Completed)
        .expect("complete");
    todo.set_status(second, TodoStatus::InProgress)
        .expect("start second");
}

#[test]
fn plan_is_versioned_and_approval_is_explicit() {
    let mut plan = Plan::new();
    plan.add_node("a", &[]).expect("node");
    plan.add_node("b", &["a"]).expect("node");
    assert_eq!(plan.ready_nodes(), vec!["a".to_owned()]);
    let revision = plan.approve().expect("approve");
    assert_eq!(revision, 1);
    assert!(plan.is_approved());
}

#[test]
fn goal_budget_pauses_and_completion_records_assurance() {
    let mut goal = Goal::new(Some(10));
    goal.consume(10).expect("consume");
    assert_eq!(goal.status(), GoalStatus::Paused);
    goal.complete(Assurance::Verified).expect("complete");
    assert_eq!(goal.status(), GoalStatus::Complete);
    assert_eq!(goal.assurance(), Some(Assurance::Verified));
}
