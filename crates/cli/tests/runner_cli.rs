//! The bridge runner's commands as processes (runner spec 1.1, 1.2): `runner init`, `runner pin`,
//! `bridge-serve`'s pin check and lock, `runner status` and `runner launchagent`, against a
//! connector stand-in. The stand-in reports the executable hash its `--config` file holds, so a
//! test swaps the connector by rewriting that file, and it notes each `--init` beside it.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_issue-dashboard");

const CONNECTOR: &str = r#"#!/usr/bin/env python3
import json, os, sys, time
args = sys.argv[1:]
cfg = args[args.index("--config") + 1]
sha = open(cfg).read().strip()
if "--init" in args:
    with open(os.path.join(os.path.dirname(cfg), "inits"), "a") as f:
        f.write("init\n")
for line in sys.stdin:
    req = json.loads(line)
    m, p = req["method"], req.get("params", {})
    if m == "capabilities":
        res = {"connector_contract_version": 2, "package_version": "0.2.4", "executable": {"status": "ok", "sha256": sha}}
    elif m == "health":
        res = {"harnesses": []}
    elif m == "wait":
        time.sleep(min(p.get("timeout_ms", 0), 200) / 1000)
        res = []
    else:
        res = {}
    sys.stdout.write(json.dumps({"schema_version": 1, "id": req["id"], "result": res}) + "\n")
    sys.stdout.flush()
"#;

struct Runner {
    dir: tempfile::TempDir,
}

impl Runner {
    fn new() -> Runner {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        fs::create_dir_all(p.join("projects")).unwrap();
        fs::write(p.join("repos.toml"), "[repo.\"example-org/server\"]\n").unwrap();
        fs::write(
            p.join("projects/home.toml"),
            "schema = 1\nname = \"Home\"\nowner_id = \"user_owner\"\n[[member]]\nrepo = \"example-org/server\"\n",
        )
        .unwrap();
        fs::create_dir_all(p.join("connector")).unwrap();
        let exe = p.join("connector/agentbridge-dashboard");
        fs::write(&exe, CONNECTOR).unwrap();
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
        let r = Runner { dir };
        r.connector_is("aa");
        r
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn config(&self) -> PathBuf {
        self.path().join("connector/config.toml")
    }

    /// The connector now reports executable hash `sha`.
    fn connector_is(&self, sha: &str) {
        fs::write(self.config(), sha).unwrap();
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(BIN);
        c.args(args).env("ISSUE_DASHBOARD_ROOT", self.path());
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn init(&self) -> Output {
        let exe = self.path().join("connector/agentbridge-dashboard");
        let state = self.path().join("connector/state");
        self.run(&[
            "runner",
            "init",
            "--connector",
            exe.to_str().unwrap(),
            "--config",
            self.config().to_str().unwrap(),
            "--state-dir",
            state.to_str().unwrap(),
        ])
    }

    fn inits(&self) -> usize {
        fs::read_to_string(self.path().join("connector/inits")).map_or(0, |s| s.lines().count())
    }

    fn json(&self, args: &[&str]) -> Value {
        let o = self.run(args);
        assert!(o.status.success(), "{args:?}: {}", String::from_utf8_lossy(&o.stderr));
        serde_json::from_slice(&o.stdout).unwrap()
    }

    fn serve(&self) -> Child {
        self.command(&["bridge-serve"])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn init_starts_the_connector_with_init_once_and_refuses_a_second_time() {
    let r = Runner::new();
    let o = r.init();
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(r.inits(), 1);
    assert!(r.path().join("runner.json").exists());
    let o = r.init();
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    assert!(stderr(&o).contains("already set up"), "{}", stderr(&o));
    assert_eq!(r.inits(), 1, "a refused init never starts the connector");
}

#[test]
fn bridge_serve_refuses_to_start_unpinned_and_against_a_connector_that_is_not_the_pinned_one() {
    let r = Runner::new();
    assert!(r.init().status.success());
    let o = r.run(&["bridge-serve"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("runner pin"), "{}", stderr(&o));
    let pinned = r.json(&["runner", "pin"]);
    assert_eq!(pinned["setup"]["pin"]["executable_sha256"], "aa");
    r.connector_is("bb");
    let o = r.run(&["bridge-serve"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("not the one the runner is pinned to"), "{}", stderr(&o));
}

#[test]
fn a_running_service_ticks_and_a_second_one_is_refused_naming_the_holder_s_pid() {
    let r = Runner::new();
    assert!(r.init().status.success());
    r.json(&["runner", "pin"]);
    let mut first = r.serve();
    let deadline = Instant::now() + Duration::from_secs(20);
    // Before the service's first write there is no ledger, and status is refused.
    let status = loop {
        let o = r.run(&["runner", "status"]);
        let s: Value = serde_json::from_slice(&o.stdout).unwrap_or(Value::Null);
        if o.status.success() && !s["status"].is_null() {
            break s;
        }
        if let Ok(Some(exit)) = first.try_wait() {
            panic!("bridge-serve exited {exit} before its first tick");
        }
        assert!(Instant::now() < deadline, "no tick within 20 s");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(status["down"], false);
    assert_eq!(status["status"]["pid"], first.id());
    assert_eq!(status["status"]["version"], "0.2.4");
    let second = r.run(&["bridge-serve"]);
    let pinning = r.run(&["runner", "pin"]);
    first.kill().unwrap();
    first.wait().unwrap();
    assert!(!second.status.success());
    assert!(stderr(&second).contains(&format!("pid={}", first.id())), "{}", stderr(&second));
    assert!(!pinning.status.success(), "pin never starts a second connector beside the service");
}

#[test]
fn launchagent_prints_a_definition_that_runs_bridge_serve_for_this_root() {
    let r = Runner::new();
    let v = r.json(&["runner", "launchagent"]);
    let text = v["text"].as_str().unwrap();
    assert!(text.contains("bridge-serve"), "{text}");
    assert!(text.contains(r.path().to_str().unwrap()), "{text}");
    assert!(!Path::new(v["install_path"].as_str().unwrap()).exists() || v["install_path"].as_str().unwrap().starts_with('~'));
}
