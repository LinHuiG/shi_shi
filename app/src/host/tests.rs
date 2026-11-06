use super::*;

#[test]
fn events_before_connect_are_kept_in_order() {
    let host = AppHost::new(PathBuf::from("unused"), PathBuf::from("unused"));
    host.event(HostEvent::Open("a".into()));
    host.event(HostEvent::Exit { restart: false });
    let (tx, rx) = std::sync::mpsc::channel();
    let tx = Mutex::new(tx);
    host.connect(move |e| tx.lock().unwrap().send(e).unwrap());
    host.event(HostEvent::Open("b".into()));
    let seen: Vec<_> = rx.try_iter().collect();
    assert_eq!(seen, [HostEvent::Open("a".into()), HostEvent::Exit { restart: false }, HostEvent::Open("b".into())]);
}
