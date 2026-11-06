use super::*;

fn collect(input: &[u8], line_limit: usize, total_limit: u64) -> Vec<Result<Vec<u8>, String>> {
    let (tx, rx) = mpsc::sync_channel(QUEUED_LINES);
    read_lines(input, line_limit, total_limit, &tx);
    drop(tx);
    rx.iter()
        .map(|p| match p {
            Piece::Line(b) => Ok(b),
            Piece::Fail(e) => Err(e),
        })
        .collect()
}

#[test]
fn lines_keep_multibyte_text_whole_and_deliver_the_unterminated_tail() {
    let pieces = collect("中🙂文\n尾".as_bytes(), 100, 100);
    assert_eq!(pieces, [Ok("中🙂文\n".as_bytes().to_vec()), Ok("尾".as_bytes().to_vec())]);
}

#[test]
fn output_and_line_sizes_are_bounded() {
    let pieces = collect(b"abcdefghijkl\n", 8, 100);
    assert!(matches!(pieces.as_slice(), [Err(e)] if e.contains("单行")));
    let pieces = collect(b"abc\ndef\nghi\n", 8, 8);
    assert_eq!(pieces.len(), 3);
    assert!(matches!(&pieces[2], Err(e) if e == TOO_LARGE));
    let (tx, rx) = mpsc::sync_channel(1);
    let errors = Mutex::new(Vec::new());
    read_stderr(&b"0123456789"[..], &errors, 4, &tx);
    assert!(matches!(rx.try_recv(), Ok(Piece::Fail(e)) if e == TOO_LARGE));
}

#[test]
fn already_cancelled_request_never_launches() {
    let flag = AtomicBool::new(true);
    let error =
        run_cancellable(Command::new("nonexistent-shishi"), None, Duration::from_secs(5), Some(&flag), &mut |_| {})
            .unwrap_err();
    assert_eq!(error, CANCELLED);
}

#[cfg(unix)]
#[test]
fn lines_stream_and_interrupt_stops_the_child() {
    let mut cmd = Command::new("/bin/sh");
    cmd.args(["-c", "read x; echo \"got $x\"; echo; echo second"]);
    let mut lines = vec![];
    let (out, _) =
        run_cancellable(cmd, Some(b"in\n"), Duration::from_secs(10), None, &mut |l| lines.push(l.to_string())).unwrap();
    assert_eq!(lines, ["got in", "second"]);
    assert_eq!(out, "got in\n\nsecond\n");

    let flag = AtomicBool::new(false);
    let mut cmd = Command::new("/bin/sh");
    cmd.args(["-c", "echo ready; sleep 30"]);
    let start = Instant::now();
    let error =
        run_cancellable(cmd, None, Duration::from_secs(20), Some(&flag), &mut |_| flag.store(true, Ordering::SeqCst))
            .unwrap_err();
    assert_eq!(error, CANCELLED);
    assert!(start.elapsed() < Duration::from_secs(10));

    let mut cmd = Command::new("sleep");
    cmd.arg("30");
    let start = Instant::now();
    let error = run(cmd, None, Duration::from_millis(200)).unwrap_err();
    assert!(error.contains("已停止本次子进程"));
    assert!(start.elapsed() < Duration::from_secs(5));

    let mut cmd = Command::new("/bin/sh");
    cmd.args(["-c", "echo bad >&2; exit 3"]);
    assert!(run(cmd, None, Duration::from_secs(5)).unwrap_err().ends_with("：bad\n"));
}

#[cfg(unix)]
#[test]
fn a_descendant_holding_the_pipe_does_not_hold_the_result() {
    // The shell exits at once; its background child keeps stdout open.
    let mut cmd = Command::new("/bin/sh");
    cmd.args(["-c", "echo done; sleep 30 &"]);
    let start = Instant::now();
    let (out, _) = run(cmd, None, Duration::from_secs(20)).unwrap();
    assert_eq!(out, "done\n");
    assert!(start.elapsed() < Duration::from_secs(10));
}

#[cfg(unix)]
#[test]
fn a_child_that_closes_its_pipes_still_obeys_the_time_limit() {
    let mut cmd = Command::new("/bin/sh");
    cmd.args(["-c", "exec >&- 2>&-; sleep 30"]);
    let start = Instant::now();
    let error = run(cmd, None, Duration::from_millis(300)).unwrap_err();
    assert!(error.contains("已停止本次子进程"));
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[cfg(unix)]
#[test]
fn a_large_stderr_never_blocks_the_child() {
    let mut cmd = Command::new("/bin/sh");
    cmd.args(["-c", "head -c 1000000 /dev/zero >&2; echo ok"]);
    let (out, err) = run(cmd, None, Duration::from_secs(20)).unwrap();
    assert_eq!(out, "ok\n");
    assert_eq!(err.len(), 1_000_000);
}
