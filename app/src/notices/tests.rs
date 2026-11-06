use super::*;

fn notice(id: &str) -> Notice {
    Notice::from_event(&HostEvent::Notify { title: id.into(), body: "已更新".into(), task_id: Some(id.into()) })
        .unwrap()
}

#[test]
fn every_change_is_a_new_notice_newest_first() {
    let mut q = Queue::default();
    for id in ["a", "b", "c", "a"] {
        q.push(notice(id));
    }
    let at = Instant::now();
    let shown = q.active(at, 3);
    assert_eq!(shown.iter().map(|n| n.title.as_str()).collect::<Vec<_>>(), ["a", "c", "b"]);
    assert!(q.deadline().is_none(), "nothing expires before it is shown");
    for n in &shown {
        q.shown(&n.id, at);
    }
    assert_eq!(q.deadline(), Some(at + LIFETIME));
    assert_eq!(q.active(at + LIFETIME - Duration::from_millis(1), 3).len(), 3);
    let waiting = q.active(at + LIFETIME, 3);
    assert_eq!(waiting.iter().map(|n| n.title.as_str()).collect::<Vec<_>>(), ["a"]);
    q.close("missing");
    assert_eq!(q.count(), 1);
    q.push(notice("a"));
    q.push(notice("b"));
    q.dismiss_task("a");
    assert_eq!(q.active(at, 3).iter().map(|n| n.title.as_str()).collect::<Vec<_>>(), ["b"]);
}

#[test]
fn notices_without_a_task_get_a_random_id_and_only_show_the_window() {
    let event = HostEvent::Notify { title: "t".into(), body: "b".into(), task_id: None };
    let (a, b) = (Notice::from_event(&event).unwrap(), Notice::from_event(&event).unwrap());
    assert!(a.task.is_none() && a.id != b.id);
    assert!(Notice::from_event(&HostEvent::Open("x".into())).is_none());
    assert_eq!(serde_json::to_value(&a).unwrap()["summary"], "b");
}
