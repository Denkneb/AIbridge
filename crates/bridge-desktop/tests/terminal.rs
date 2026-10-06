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
