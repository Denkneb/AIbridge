use bridge_automation::codex::{CodexClient, CodexError, LOG_LIMIT, Operation, validate_answer};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    time::{Duration, Instant},
};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("bridge-codex-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("workspace")).unwrap();
        fs::write(root.join("fixture.py"), r#"import sys,json,time,os,subprocess
mode=sys.argv[1]
args=sys.argv[2:]
schema=args[args.index('--output-schema')+1]
result=args[args.index('-o')+1]
assert '--ignore-user-config' in args
assert args[args.index('--sandbox')+1]=='read-only'
assert args[args.index('--disable')+1]=='multi_agent'
assert json.load(open(schema))['additionalProperties'] is False
context=json.load(sys.stdin)
if mode=='nonzero': sys.exit(5)
if mode=='hang': time.sleep(30)
if mode=='descendant':
    subprocess.Popen([sys.executable,'-c','import time;time.sleep(30)'])
if mode=='flood':
    sys.stdout.buffer.write(b'x'*(9*1024*1024));sys.stdout.flush();time.sleep(30)
if mode=='oversize': open(result,'w').write('x'*1000001)
elif mode=='invalid': open(result,'w').write('bad-json')
elif mode=='symlink':
    os.unlink(result);os.symlink(schema,result)
else:
    answer={'task':'Inspect approved step'} if context['operation']=='prepare' else {'decision':'accept','summary':'Reviewed','findings':[]}
    json.dump(answer,open(result,'w'))
"#).unwrap();
        Self(root)
    }
    fn client(&self, mode: &str, timeout: Duration) -> CodexClient {
        CodexClient::new(
            self.0.join("private"),
            timeout,
            Some("fixture/model".into()),
        )
        .unwrap()
        .with_executable(
            std::path::Path::new("/usr/bin/python3"),
            vec![self.0.join("fixture.py").into(), mode.into()],
        )
        .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn expand(value: &Value) -> Value {
    if let (Some(text), Some(count)) = (
        value.get("repeat_text").and_then(Value::as_str),
        value.get("count").and_then(Value::as_u64),
    ) {
        return json!(text.repeat(count as usize));
    }
    match value {
        Value::Array(values) => Value::Array(values.iter().map(expand).collect()),
        Value::Object(values) => {
            Value::Object(values.iter().map(|(k, v)| (k.clone(), expand(v))).collect())
        }
        _ => value.clone(),
    }
}
#[test]
fn pinned_structured_answer_corpus() {
    let corpus: Value = serde_json::from_str(include_str!(
        "../../../docs/fixtures/runtime-automation-v17.json"
    ))
    .unwrap();
    let mut count = 0;
    for case in corpus["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["operation"] == "answer")
    {
        let kind = if case["kind"] == "prepare" {
            Operation::Prepare
        } else {
            Operation::Review
        };
        let answer = validate_answer(kind, expand(&case["input"]));
        if let Some(expected) = case["expect"].get("exact") {
            assert_eq!(&answer.unwrap(), expected, "{}", case["id"]);
        } else {
            assert_eq!(answer.err(), Some(CodexError::Result), "{}", case["id"]);
        }
        count += 1;
    }
    assert!(count >= 15);
}
#[test]
fn readonly_argv_json_input_and_private_artifacts() {
    let fixture = Fixture::new();
    for kind in [Operation::Prepare, Operation::Review] {
        let answer = fixture
            .client("valid", Duration::from_secs(3))
            .call(
                kind,
                &fixture.0.join("workspace"),
                &json!({"goal":"approved"}),
                || false,
            )
            .unwrap();
        assert!(answer.is_object());
    }
    for file in fs::read_dir(fixture.0.join("private")).unwrap() {
        assert_eq!(
            file.unwrap().metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
#[test]
fn invalid_nonzero_oversized_and_symlink_outputs_refuse_acceptance() {
    let fixture = Fixture::new();
    for (mode, expected) in [
        ("invalid", CodexError::Result),
        ("nonzero", CodexError::Exit),
        ("oversize", CodexError::Result),
        ("symlink", CodexError::Result),
    ] {
        assert_eq!(
            fixture.client(mode, Duration::from_secs(3)).call(
                Operation::Review,
                &fixture.0.join("workspace"),
                &json!({}),
                || false
            ),
            Err(expected)
        );
    }
}
#[test]
fn timeout_cancellation_and_descendant_cleanup_are_bounded() {
    let fixture = Fixture::new();
    let started = Instant::now();
    assert_eq!(
        fixture.client("hang", Duration::from_millis(100)).call(
            Operation::Review,
            &fixture.0.join("workspace"),
            &json!({}),
            || false
        ),
        Err(CodexError::Timeout)
    );
    assert_eq!(
        fixture.client("hang", Duration::from_secs(3)).call(
            Operation::Review,
            &fixture.0.join("workspace"),
            &json!({}),
            || started.elapsed() > Duration::from_millis(200)
        ),
        Err(CodexError::Cancelled)
    );
    assert!(
        fixture
            .client("descendant", Duration::from_secs(3))
            .call(
                Operation::Review,
                &fixture.0.join("workspace"),
                &json!({}),
                || false
            )
            .is_ok()
    );
    assert!(started.elapsed() < Duration::from_secs(5));
}
#[test]
fn logs_are_capped_and_checkout_artifact_directory_is_refused() {
    let fixture = Fixture::new();
    assert_eq!(
        fixture.client("flood", Duration::from_secs(3)).call(
            Operation::Review,
            &fixture.0.join("workspace"),
            &json!({}),
            || false
        ),
        Err(CodexError::LogLimit)
    );
    for file in fs::read_dir(fixture.0.join("private")).unwrap() {
        assert!(file.unwrap().metadata().unwrap().len() <= LOG_LIMIT as u64);
    }
    let client = CodexClient::new(
        fixture.0.join("workspace/private"),
        Duration::from_secs(1),
        None,
    )
    .unwrap();
    assert_eq!(
        client.call(
            Operation::Prepare,
            &fixture.0.join("workspace"),
            &json!({}),
            || false
        ),
        Err(CodexError::Input)
    );
}
