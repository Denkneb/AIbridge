use bridge_desktop::terminal::{Event, Terminals};
use portable_pty::CommandBuilder;
use std::{
    thread,
    time::{Duration, Instant},
};
#[test]
fn real_pty_input_unicode_resize_and_ordered_large_output() {
    let terms = Terminals::default();
    let mut cmd = CommandBuilder::new("/bin/bash");
    cmd.args([
        "--noprofile",
        "--norc",
        "-c",
        "read value; printf 'VALUE:%s\\n' \"$value\"; stty size; python3 -c 'print(\"ю\"*100000)' ",
    ]);
    let id = terms.open(cmd, 24, 80).unwrap();
    terms.resize(&id, 37, 101).unwrap();
    terms.write(&id, "привет\n".as_bytes().to_vec()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut bytes = vec![];
    let mut sequence = 0;
    loop {
        assert!(Instant::now() < deadline);
        for e in terms.read(&id).unwrap() {
            match e {
                Event::Data {
                    sequence: n,
                    bytes: b,
                } => {
                    assert_eq!(n, sequence);
                    sequence += 1;
                    bytes.extend(b);
                }
                Event::Exit { code } => {
                    assert_eq!(code, 0);
                    let text = String::from_utf8(bytes).unwrap();
                    assert!(text.contains("VALUE:привет"));
                    assert!(text.contains("37 101"));
                    assert!(text.matches('ю').count() >= 100000);
                    terms.close(&id).unwrap();
                    return;
                }
                Event::Error { message } => panic!("{message}"),
            }
        }
        thread::sleep(Duration::from_millis(5));
    }
}
#[test]
fn close_full_output_queue_is_bounded_and_repeatable() {
    let terms = Terminals::default();
    let mut cmd = CommandBuilder::new("/bin/bash");
    cmd.args([
        "--noprofile",
        "--norc",
        "-c",
        "while true; do printf 'busy busy busy busy busy busy busy busy\\n'; done",
    ]);
    let id = terms.open(cmd, 24, 80).unwrap();
    thread::sleep(Duration::from_millis(150));
    let start = Instant::now();
    terms.close(&id).unwrap();
    assert!(start.elapsed() < Duration::from_secs(2));
    terms.close(&id).unwrap();
    assert!(terms.write(&id, vec![1]).is_err());
    assert!(terms.resize(&id, 0, 80).is_err());
}

#[test]
fn closing_one_real_pty_preserves_the_other_process_and_input() {
    let terms = Terminals::default();
    let mut cmd = CommandBuilder::new("/bin/bash");
    cmd.args([
        "--noprofile",
        "--norc",
        "-c",
        "while read value; do printf 'ALIVE:%s\\n' \"$value\"; done",
    ]);
    let first = terms.open(cmd.clone(), 24, 80).unwrap();
    let second = terms.open(cmd, 24, 80).unwrap();
    assert_ne!(first, second);
    terms.close(&second).unwrap();
    terms.write(&first, b"after-close\n".to_vec()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut output = Vec::new();
    loop {
        assert!(Instant::now() < deadline, "surviving PTY did not respond");
        for event in terms.read(&first).unwrap() {
            match event {
                Event::Data { bytes, .. } => output.extend(bytes),
                Event::Exit { .. } => panic!("closing a peer terminated this PTY"),
                Event::Error { message } => panic!("{message}"),
            }
        }
        if String::from_utf8_lossy(&output).contains("ALIVE:after-close") {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    terms.close(&first).unwrap();
}

#[test]
fn shutdown_cleans_terminal_child_and_blocks_reopen() {
    let terms = Terminals::default();
    let mut cmd = CommandBuilder::new("/bin/bash");
    cmd.args([
        "--noprofile",
        "--norc",
        "-c",
        "trap '' TERM; sleep 90 & echo CHILD:$!; wait",
    ]);
    let id = terms.open(cmd, 24, 80).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut output = Vec::new();
    let child = loop {
        assert!(Instant::now() < deadline);
        for event in terms.read(&id).unwrap() {
            if let Event::Data { bytes, .. } = event {
                output.extend(bytes);
            }
        }
        let text = String::from_utf8_lossy(&output);
        if let Some(tail) = text.split("CHILD:").nth(1)
            && let Some(line) = tail.split_once('\n')
        {
            break line.0.trim().parse::<i32>().unwrap();
        }
        thread::sleep(Duration::from_millis(10));
    };
    terms.shutdown().unwrap();
    let stat = std::fs::read_to_string(format!("/proc/{child}/stat")).ok();
    assert!(stat.is_none_or(|s| s.rsplit_once(") ").unwrap().1.starts_with('Z')));
    assert!(terms.read(&id).is_err());
    assert_eq!(
        terms
            .open(CommandBuilder::new("/bin/bash"), 24, 80)
            .unwrap_err(),
        "application is shutting down"
    );
    terms.shutdown().unwrap();
}

#[test]
fn external_terminal_launcher_is_owned_and_shutdown_blocks_new_launches() {
    let terms = Terminals::default();
    let root = std::env::temp_dir().join(format!("bridge-external-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let path = root.join("pid");
    let mut cmd = std::process::Command::new("python3");
    cmd.args([
        "-c",
        "import os,time,sys; open(sys.argv[1],'w').write(str(os.getpid())); time.sleep(90)",
    ])
    .arg(&path);
    terms.open_external(cmd).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
    let pid: i32 = std::fs::read_to_string(&path).unwrap().parse().unwrap();
    terms.shutdown().unwrap();
    assert!(
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .is_none_or(|s| s.rsplit_once(") ").unwrap().1.starts_with('Z'))
    );
    assert_eq!(
        terms
            .open_external(std::process::Command::new("/bin/true"))
            .unwrap_err(),
        "application is shutting down"
    );
    std::fs::remove_dir_all(root).unwrap();
}
