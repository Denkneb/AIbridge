use bridge_runtime::{RuntimeError, process_tree::ProcessTree};
use std::{
    fs,
    process::{Command, Stdio},
    time::{Duration, Instant},
};
fn live(pid: i32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| {
            s.rsplit_once(") ")
                .map(|(_, rest)| rest.starts_with('Z') || rest.starts_with('X'))
        })
        .is_some_and(|dead| !dead)
}
#[test]
fn stop_cleans_detached_descendants_and_preserves_unrelated_process() {
    let root = std::env::temp_dir().join(format!(
        "bridge-process-tree-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&root).unwrap();
    let mut child = Command::new("python3").args(["-c", "import os,signal,subprocess,time,sys; signal.signal(signal.SIGTERM,signal.SIG_IGN); p=subprocess.Popen(['python3','-c','import os,signal,time; os.setsid(); signal.signal(signal.SIGTERM,signal.SIG_IGN); time.sleep(90)']); open(sys.argv[1],'w').write(str(p.pid)); time.sleep(90)"]).arg(root.join("pid")).stdin(Stdio::null()).spawn().unwrap();
    let tree = ProcessTree::capture(child.id()).unwrap();
    let mut peer = Command::new("sleep").arg("90").spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !root.join("pid").exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let descendant: i32 = fs::read_to_string(root.join("pid"))
        .unwrap()
        .parse()
        .unwrap();
    assert!(live(descendant));
    tree.stop().unwrap();
    child.wait().unwrap();
    assert!(!live(descendant));
    assert!(peer.try_wait().unwrap().is_none());
    tree.stop().unwrap();
    peer.kill().unwrap();
    peer.wait().unwrap();
    fs::remove_dir_all(root).unwrap();
}
#[test]
fn forged_identity_cannot_signal_an_unrelated_process() {
    let mut child = Command::new("sleep").arg("90").spawn().unwrap();
    let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap();
    assert!(matches!(
        ProcessTree::from_identity(child.id() as i32, "0", boot.trim()),
        Err(RuntimeError::ForeignProcess)
    ));
    assert!(child.try_wait().unwrap().is_none());
    child.kill().unwrap();
    child.wait().unwrap();
}
