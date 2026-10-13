//! Start a child omp session for a prepared checkout in a new Tern tab or split.
//! Nothing dispatched (no `tern` binary) is a manual result; a failed or
//! malformed response after dispatch is unknown. Never retries.

use std::io::ErrorKind;
use std::path::Path;

use crate::model::{
    LAUNCH_MESSAGE, LaunchPlacement, LaunchResult, OmpConfig, PreparedCheckout, RUN_TIMEOUT,
};
use crate::process;
use crate::text::{sanitize_for_terminal, truncate_chars};

const MESSAGE_LIMIT: usize = 300;

fn safe_word(word: &str) -> bool {
    !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c))
}

/// POSIX single-quote rendering of argv for display only; never submitted to a shell.
pub fn posix_quote(argv: &[String]) -> String {
    argv.iter()
        .map(|word| {
            if safe_word(word) {
                word.clone()
            } else {
                format!("'{}'", word.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn message(text: &str) -> String {
    let line = sanitize_for_terminal(text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    truncate_chars(&line, MESSAGE_LIMIT)
}

pub fn omp_argv(omp: &OmpConfig, cwd: &str, snapshot_path: &Path) -> Vec<String> {
    vec![
        omp.binary.clone(),
        "--cwd".to_string(),
        cwd.to_string(),
        "--profile".to_string(),
        omp.profile.clone(),
        format!("@{}", snapshot_path.display()),
        "--".to_string(),
        LAUNCH_MESSAGE.to_string(),
    ]
}

pub fn tern_argv(placement: LaunchPlacement, cwd: &str, command: &[String]) -> Vec<String> {
    let head: &[&str] = match placement {
        LaunchPlacement::Tab => &["new", "tab"],
        LaunchPlacement::Split => &["split", "@focused", "right"],
    };
    head.iter()
        .copied()
        .chain(["--cwd", cwd, "--json", "--"])
        .map(str::to_string)
        .chain(command.iter().cloned())
        .collect()
}

fn tern_binary() -> String {
    std::env::var("TERN_BIN")
        .ok()
        .filter(|bin| !bin.is_empty())
        .unwrap_or_else(|| "tern".to_string())
}

/// Block id from Tern's JSON reply (`id`, `block`, `blockId` or `block_id`).
fn block_id(reply: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    ["id", "block", "blockId", "block_id"]
        .iter()
        .find_map(|name| match reply.get(*name) {
            Some(serde_json::Value::String(id)) => Some(message(id)),
            Some(serde_json::Value::Number(id)) => Some(id.to_string()),
            _ => None,
        })
}

pub fn launch_omp(
    omp: &OmpConfig,
    prepared: &PreparedCheckout,
    snapshot_path: &Path,
    placement: LaunchPlacement,
) -> LaunchResult {
    let cwd = prepared.path.clone();
    let argv = omp_argv(omp, &cwd, snapshot_path);
    let tern = tern_binary();
    let tern_args = tern_argv(placement, &cwd, &argv);
    let args: Vec<&str> = tern_args.iter().map(String::as_str).collect();
    let unknown = |reason: String| LaunchResult::Unknown {
        argv: argv.clone(),
        cwd: cwd.clone(),
        reason: message(&reason),
    };

    let output = match process::run(&tern, &args, Some(Path::new(&cwd)), RUN_TIMEOUT, None) {
        Ok(output) => output,
        Err(error) => {
            let reason = if error.kind() == ErrorKind::NotFound {
                format!("Tern 실행 파일({tern})을 찾을 수 없습니다. 아래 명령을 직접 실행하세요.")
            } else {
                format!("Tern 실행 파일({tern})을 실행하지 못했습니다: {error}")
            };
            return LaunchResult::Manual {
                argv: argv.clone(),
                cwd: cwd.clone(),
                reason: message(&reason),
            };
        }
    };
    if output.killed {
        return unknown(format!(
            "Tern 실행 요청이 {}초 안에 끝나지 않았습니다.",
            RUN_TIMEOUT.as_secs()
        ));
    }
    if output.code != Some(0) {
        let code = output
            .code
            .map_or_else(|| "signal".to_string(), |c| c.to_string());
        let detail = output.stderr.trim();
        return unknown(format!(
            "Tern 실행 요청이 실패했습니다 (exit {code}): {detail}"
        ));
    }
    match serde_json::from_str::<serde_json::Value>(output.stdout.trim()) {
        Ok(serde_json::Value::Object(reply)) => {
            let warning = match reply.get("warning") {
                Some(serde_json::Value::String(w)) => Some(message(w)),
                _ => None,
            };
            LaunchResult::Requested {
                block: block_id(&reply),
                warning,
            }
        }
        _ => unknown("Tern 실행 응답 형식이 올바르지 않습니다.".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handoff::tests::temp_dir;
    use crate::model::{Backend, Checkout, Repository};
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn omp() -> OmpConfig {
        OmpConfig {
            binary: "omp".into(),
            profile: "default".into(),
        }
    }

    fn prepared(path: &str) -> PreparedCheckout {
        PreparedCheckout {
            repository: Repository {
                backend: Backend::Git,
                key: "/repo/.git".into(),
                root: "/repo".into(),
                name: "repo".into(),
            },
            checkout: Checkout {
                repository_key: "/repo/.git".into(),
                name: "TEST-1".into(),
                path: Some(path.into()),
                head: None,
                detached: false,
                source: false,
                locked: false,
                prunable: false,
                unavailable_reason: None,
            },
            path: path.into(),
            created: true,
        }
    }

    /// Fake `tern` that records argv (one per line) and replies with `body`/`code`.
    fn fake_tern(dir: &Path, stdout: &str, code: i32) -> (PathBuf, PathBuf) {
        let log = dir.join("argv.log");
        let script = dir.join("tern");
        let text = format!(
            "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\" >> '{}'; done\nprintf '%s' '{}'\nexit {code}\n",
            log.display(),
            stdout
        );
        std::fs::write(&script, text).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        (script, log)
    }

    fn launch_with(bin: &Path, cwd: &str, placement: LaunchPlacement) -> LaunchResult {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var_os("TERN_BIN");
        // SAFETY: env mutation is serialized by ENV_LOCK within this test module.
        unsafe { std::env::set_var("TERN_BIN", bin) };
        let result = launch_omp(
            &omp(),
            &prepared(cwd),
            Path::new("/state/handoffs/a.md"),
            placement,
        );
        match previous {
            Some(value) => unsafe { std::env::set_var("TERN_BIN", value) },
            None => unsafe { std::env::remove_var("TERN_BIN") },
        }
        result
    }

    #[test]
    fn argv_shapes() {
        let command = omp_argv(&omp(), "/w/TEST-1", Path::new("/s/h.md"));
        assert_eq!(
            command,
            [
                "omp",
                "--cwd",
                "/w/TEST-1",
                "--profile",
                "default",
                "@/s/h.md",
                "--",
                LAUNCH_MESSAGE
            ]
        );
        let tab = tern_argv(LaunchPlacement::Tab, "/w", &["omp".to_string()]);
        assert_eq!(tab, ["new", "tab", "--cwd", "/w", "--json", "--", "omp"]);
        let split = tern_argv(LaunchPlacement::Split, "/w", &["omp".to_string()]);
        assert_eq!(
            split,
            [
                "split", "@focused", "right", "--cwd", "/w", "--json", "--", "omp"
            ]
        );
    }

    #[test]
    fn exit_zero_json_is_requested_with_block_id() {
        let dir = temp_dir("jira-launch");
        let cwd = dir.to_string_lossy().to_string();
        let (bin, log) = fake_tern(&dir, r#"{"id":"blk-7"}"#, 0);
        let result = launch_with(&bin, &cwd, LaunchPlacement::Split);
        assert_eq!(
            result,
            LaunchResult::Requested {
                block: Some("blk-7".into()),
                warning: None
            }
        );
        let recorded: Vec<String> = std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        let expected = tern_argv(
            LaunchPlacement::Split,
            &cwd,
            &omp_argv(&omp(), &cwd, Path::new("/state/handoffs/a.md")),
        );
        assert_eq!(recorded, expected);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn non_zero_exit_is_unknown() {
        let dir = temp_dir("jira-launch");
        let cwd = dir.to_string_lossy().to_string();
        let (bin, log) = fake_tern(&dir, "", 3);
        let result = launch_with(&bin, &cwd, LaunchPlacement::Tab);
        assert!(matches!(&result, LaunchResult::Unknown { argv, .. } if argv[0] == "omp"));
        // Sent once, never retried.
        let lines = std::fs::read_to_string(&log).unwrap();
        assert_eq!(lines.lines().filter(|l| *l == "new").count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn malformed_reply_is_unknown() {
        let dir = temp_dir("jira-launch");
        let cwd = dir.to_string_lossy().to_string();
        let (bin, _) = fake_tern(&dir, "not json", 0);
        assert!(matches!(
            launch_with(&bin, &cwd, LaunchPlacement::Tab),
            LaunchResult::Unknown { .. }
        ));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_binary_is_manual() {
        let dir = temp_dir("jira-launch");
        let cwd = dir.to_string_lossy().to_string();
        let result = launch_with(&dir.join("no-such-tern"), &cwd, LaunchPlacement::Tab);
        match result {
            LaunchResult::Manual {
                argv,
                cwd: manual_cwd,
                ..
            } => {
                assert_eq!(
                    argv,
                    omp_argv(&omp(), &cwd, Path::new("/state/handoffs/a.md"))
                );
                assert_eq!(manual_cwd, cwd);
            }
            other => panic!("expected manual, got {other:?}"),
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn posix_quote_quotes_unsafe_words() {
        let argv = vec![
            "omp".to_string(),
            "it's here".to_string(),
            "@/a/b.md".to_string(),
        ];
        assert_eq!(posix_quote(&argv), "omp 'it'\\''s here' @/a/b.md");
    }
}
