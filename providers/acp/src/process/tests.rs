use super::*;

#[test]
fn expired_and_cancelled_commands_are_killed() {
    for cancelled in [true, false] {
        let mut command = Command::new(if cfg!(windows) { "cmd.exe" } else { "sleep" });
        if cfg!(windows) {
            command.args(["/C", "ping -n 30 127.0.0.1 >nul"]);
        } else {
            command.arg("30");
        }
        let started = Instant::now();
        let deadline = started + Duration::from_millis(if cancelled { 30_000 } else { 200 });
        let error = output_until(command, deadline, &AtomicBool::new(cancelled)).unwrap_err();
        assert_eq!(error.ends_with("取消"), cancelled);
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
