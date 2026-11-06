use super::*;

fn launch(root: &std::path::Path, background: bool) -> Result<Option<Arc<Owner>>, String> {
    let mut a =
        crate::args::parse(["--root".into(), root.display().to_string(), "--no-background-jobs".into()]).unwrap();
    a.background = background;
    start(a, root.to_path_buf(), root.join("data"))
}

#[test]
fn second_launch_forwards_to_the_owner_and_never_opens_the_data() {
    let root = wire::test_dir("owner");
    let owner = launch(&root, true).unwrap().expect("first launch owns");
    assert_eq!(owner.data, root.join("data"));
    let (tx, rx) = std::sync::mpsc::channel();
    let tx = std::sync::Mutex::new(tx);
    let service = owner.serve(move |s| tx.lock().unwrap().send(s).unwrap()).unwrap();
    assert!(launch(&root, true).unwrap().is_none(), "background launch forwards");
    assert!(owner.owners.cli());
    assert!(launch(&root, false).unwrap().is_none(), "desktop launch forwards");
    assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), Signal::Show);
    assert!(owner.owners.gui());
    let route = wire::probe(&root).unwrap().unwrap();
    let error = wire::success(wire::call(&route, "nope", json!({})).unwrap()).unwrap_err();
    assert_eq!(error, "未知请求");
    drop(service);
    drop(owner);
    wire::remove_test_dir(&root);
}
