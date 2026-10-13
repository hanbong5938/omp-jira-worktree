//! Explicit Jira status transitions: fresh lookup → (UI picker + final confirm)
//! → site+issue lock → re-read and tuple check → durable Pending → single POST.
//!
//! Jira REST has no conditional transition, so the final read→write race with
//! external Jira actors is narrowed (lock + re-read) but not eliminated.

use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::handoff::now_rfc3339;
use crate::model::{
    Issue, JiraApi, JiraError, StatusRef, Transition, TransitionResult, normalize_issue_key,
};
use crate::state::{StateError, StateStore, random_id};

#[derive(Debug)]
pub enum TransitionError {
    /// A previous request for this issue is pending or its outcome unknown; inspect Jira history first.
    Blocked {
        attempt: String,
        record_started_at: String,
    },
    /// Status or transition list changed since approval; the approval was discarded.
    Changed(String),
    /// The transition cannot be applied from here (unavailable, screen, required fields).
    Ineligible(String),
    /// The site+issue lock is held or could not be taken.
    Lock(StateError),
    State(StateError),
    Jira(JiraError),
}

impl fmt::Display for TransitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransitionError::Blocked {
                record_started_at, ..
            } => write!(
                f,
                "이전 전환 요청({record_started_at} 시작)의 결과가 불확실합니다. Jira 이력을 확인한 뒤 진행하세요."
            ),
            TransitionError::Changed(message) => f.write_str(message),
            TransitionError::Ineligible(message) => f.write_str(message),
            TransitionError::Lock(error) => write!(f, "전환 잠금 실패: {error}"),
            TransitionError::State(error) => write!(f, "전환 기록 오류: {error}"),
            TransitionError::Jira(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for TransitionError {}

/// The uncertain previous request the user is shown and must acknowledge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockedAttempt {
    pub attempt: String,
    pub started_at: String,
}

pub struct TransitionChoices {
    pub issue: Issue,
    pub transitions: Vec<Transition>,
    /// A pending/unknown previous request that blocks new transitions.
    pub blocked: Option<BlockedAttempt>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum RecordState {
    Pending,
    Acknowledged,
    Rejected,
    Unknown,
    /// The user confirmed Jira history was inspected after a pending/unknown outcome.
    Inspected,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TransitionRecord {
    version: u32,
    attempt: String,
    key: String,
    site: String,
    transition_id: String,
    from_status_id: String,
    to_status_id: String,
    state: RecordState,
    started_at: String,
}

impl TransitionRecord {
    fn uncertain(&self) -> bool {
        matches!(self.state, RecordState::Pending | RecordState::Unknown)
    }
}

pub struct TransitionService {
    jira: Arc<dyn JiraApi>,
    state: StateStore,
    site: String,
}

/// 128-bit FNV-1a-style hex digest (two independently seeded 64-bit lanes).
fn digest(text: &str) -> String {
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut a: u64 = 0xcbf2_9ce4_8422_2325;
    let mut b: u64 = 0x6c62_272e_07bb_0142;
    for byte in text.bytes() {
        a = (a ^ u64::from(byte)).wrapping_mul(PRIME);
        b = (b ^ u64::from(byte)).wrapping_mul(PRIME).rotate_left(5);
    }
    format!("{a:016x}{b:016x}")
}

impl TransitionService {
    pub fn new(jira: Arc<dyn JiraApi>, state: StateStore, site: String) -> Self {
        TransitionService { jira, state, site }
    }

    fn record_path(&self, key: &str) -> String {
        let id = serde_json::to_string(&[self.site.as_str(), key]).unwrap_or_default();
        format!("records/transitions/{}.json", digest(&id))
    }

    fn lock_key(&self, key: &str) -> String {
        format!("transition:{}:{key}", self.site)
    }

    fn read(&self, key: &str) -> Result<Option<TransitionRecord>, TransitionError> {
        let record: Option<TransitionRecord> = self
            .state
            .read_record(&self.record_path(key))
            .map_err(TransitionError::State)?;
        match record {
            Some(r) if r.version != 1 => Err(TransitionError::State(StateError::Invalid(
                "transition record: unsupported version".to_string(),
            ))),
            other => Ok(other),
        }
    }

    fn normalize(key: &str) -> Result<String, TransitionError> {
        normalize_issue_key(key).map_err(TransitionError::Ineligible)
    }

    /// Fresh issue + transitions, plus any uncertain previous request.
    pub fn load(&self, key: &str) -> Result<TransitionChoices, TransitionError> {
        let key = Self::normalize(key)?;
        let blocked = self
            .read(&key)?
            .filter(TransitionRecord::uncertain)
            .map(|r| BlockedAttempt {
                attempt: r.attempt,
                started_at: r.started_at,
            });
        let issue = self.jira.issue(&key).map_err(TransitionError::Jira)?;
        let transitions = self.jira.transitions(&key).map_err(TransitionError::Jira)?;
        Ok(TransitionChoices {
            issue,
            transitions,
            blocked,
        })
    }

    /// Whether a transition can be applied without Jira's web UI.
    pub fn eligibility(t: &Transition) -> Result<(), String> {
        if t.is_available == Some(false) {
            return Err(format!(
                "'{}' 전환은 현재 Jira에서 사용할 수 없습니다.",
                t.name
            ));
        }
        if t.has_screen {
            return Err(format!(
                "'{}' 전환은 전환 입력 화면이 필요합니다. Jira 웹에서 진행하세요.",
                t.name
            ));
        }
        if t.requires_fields {
            return Err(format!(
                "'{}' 전환은 필수 입력 필드가 있습니다. Jira 웹에서 진행하세요.",
                t.name
            ));
        }
        Ok(())
    }

    /// Applies an approved `(from, transition)` tuple: sent at most once, never retried.
    pub fn apply(
        &self,
        key: &str,
        from: &StatusRef,
        transition: &Transition,
    ) -> Result<TransitionResult, TransitionError> {
        let key = Self::normalize(key)?;
        Self::eligibility(transition).map_err(TransitionError::Ineligible)?;
        self.state
            .with_locks(&[self.lock_key(&key)], || {
                self.apply_locked(&key, from, transition)
            })
            .map_err(TransitionError::Lock)?
    }

    fn apply_locked(
        &self,
        key: &str,
        from: &StatusRef,
        transition: &Transition,
    ) -> Result<TransitionResult, TransitionError> {
        if let Some(current) = self.read(key)?.filter(TransitionRecord::uncertain) {
            return Err(TransitionError::Blocked {
                attempt: current.attempt,
                record_started_at: current.started_at,
            });
        }
        let issue = self.jira.issue(key).map_err(TransitionError::Jira)?;
        let fresh = self.jira.transitions(key).map_err(TransitionError::Jira)?;
        let same = issue.status == *from && fresh.iter().any(|t| t == transition);
        if !same {
            return Err(TransitionError::Changed(format!(
                "{key}: 확인 이후 상태나 전환 목록이 바뀌어 승인을 폐기했습니다. 다시 선택하세요."
            )));
        }

        let path = self.record_path(key);
        let pending = TransitionRecord {
            version: 1,
            attempt: random_id(),
            key: key.to_string(),
            site: self.site.clone(),
            transition_id: transition.id.clone(),
            from_status_id: from.id.clone(),
            to_status_id: transition.to.id.clone(),
            state: RecordState::Pending,
            started_at: now_rfc3339(),
        };
        self.state
            .write_record(&path, &pending)
            .map_err(TransitionError::State)?;

        let result = self.jira.post_transition(key, &transition.id);
        let (state, result) = match result {
            TransitionResult::Acknowledged { .. } => {
                let observed = self.jira.issue(key).ok().map(|i| i.status);
                let verified = observed.as_ref().is_some_and(|s| s.id == transition.to.id);
                (
                    RecordState::Acknowledged,
                    TransitionResult::Acknowledged {
                        observed_status: observed,
                        verified,
                    },
                )
            }
            TransitionResult::Rejected { message } => (
                RecordState::Rejected,
                TransitionResult::Rejected { message },
            ),
            TransitionResult::Unknown {
                message,
                observed_status,
            } => {
                let observed =
                    observed_status.or_else(|| self.jira.issue(key).ok().map(|i| i.status));
                (
                    RecordState::Unknown,
                    TransitionResult::Unknown {
                        message,
                        observed_status: observed,
                    },
                )
            }
        };
        // The request was sent; a failed outcome write leaves the record Pending, which
        // blocks the next transition until the user inspects Jira history.
        let _ = self
            .state
            .write_record(&path, &TransitionRecord { state, ..pending });
        Ok(result)
    }

    /// Records that the user inspected Jira history for the uncertain request `attempt`
    /// they were shown, unblocking the next transition for this issue. A newer uncertain
    /// attempt is never marked inspected; it is reported as `Blocked` instead.
    pub fn acknowledge_unknown(&self, key: &str, attempt: &str) -> Result<(), TransitionError> {
        let key = Self::normalize(key)?;
        self.state
            .with_locks(&[self.lock_key(&key)], || -> Result<(), TransitionError> {
                let Some(record) = self.read(&key)?.filter(TransitionRecord::uncertain) else {
                    return Ok(());
                };
                if record.attempt != attempt {
                    return Err(TransitionError::Blocked {
                        attempt: record.attempt,
                        record_started_at: record.started_at,
                    });
                }
                let inspected = TransitionRecord {
                    state: RecordState::Inspected,
                    ..record
                };
                self.state
                    .write_record(&self.record_path(&key), &inspected)
                    .map_err(TransitionError::State)
            })
            .map_err(TransitionError::Lock)?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handoff::tests::temp_dir;
    use crate::model::{Comment, CommentResult, JiraErrorKind, Page};
    use std::path::PathBuf;
    use std::sync::Mutex;

    const KEY: &str = "TEST-1";
    const SITE: &str = "https://jira.example.test";

    fn status(id: &str, name: &str) -> StatusRef {
        StatusRef {
            id: id.into(),
            name: name.into(),
        }
    }
    fn todo() -> StatusRef {
        status("10000", "To Do")
    }
    fn in_progress() -> StatusRef {
        status("3", "In Progress")
    }
    fn start() -> Transition {
        Transition {
            id: "31".into(),
            name: "Start Progress".into(),
            to: in_progress(),
            has_screen: false,
            requires_fields: false,
            is_available: Some(true),
        }
    }

    #[derive(Clone, Copy)]
    enum Post {
        /// 2xx; the server moves the issue to the target.
        Applied,
        /// 2xx without a visible status change.
        AcceptedStale,
        Rejected,
        /// 5xx after the server changed status.
        UnknownApplied,
    }

    type TransitionsHook = Box<dyn FnMut(&mut Vec<Transition>, &mut StatusRef) + Send>;

    struct Fixture {
        status: StatusRef,
        transitions: Vec<Transition>,
        posts: Vec<String>,
        post: Post,
        /// Whether a record file existed when each POST arrived.
        record_at_post: Vec<bool>,
        /// Called after each transitions GET.
        after_transitions_get: Option<TransitionsHook>,
    }

    struct FakeJira {
        fx: Mutex<Fixture>,
        record_dir: PathBuf,
    }

    impl JiraApi for FakeJira {
        fn search(&self, _jql: &str) -> Result<Page<Issue>, JiraError> {
            unreachable!()
        }
        fn issue(&self, key: &str) -> Result<Issue, JiraError> {
            if key != KEY {
                return Err(JiraError {
                    kind: JiraErrorKind::NotFound,
                    message: "nf".into(),
                    status: Some(404),
                });
            }
            Ok(Issue {
                key: KEY.into(),
                summary: "Fixture".into(),
                status: self.fx.lock().unwrap().status.clone(),
                issue_type: "Task".into(),
                assignee: String::new(),
                updated: "2026-10-01T00:00:00.000+0000".into(),
                description: String::new(),
                url: format!("{SITE}/browse/{KEY}"),
            })
        }
        fn comments(&self, _key: &str, _limit: usize) -> Result<Page<Comment>, JiraError> {
            unreachable!()
        }
        fn transitions(&self, _key: &str) -> Result<Vec<Transition>, JiraError> {
            let mut fx = self.fx.lock().unwrap();
            let list = fx.transitions.clone();
            if let Some(mut hook) = fx.after_transitions_get.take() {
                let Fixture {
                    transitions,
                    status,
                    ..
                } = &mut *fx;
                hook(transitions, status);
                fx.after_transitions_get = Some(hook);
            }
            Ok(list)
        }
        fn post_transition(&self, _key: &str, transition_id: &str) -> TransitionResult {
            let mut fx = self.fx.lock().unwrap();
            let exists = std::fs::read_dir(&self.record_dir).is_ok_and(|mut d| d.next().is_some());
            fx.record_at_post.push(exists);
            fx.posts.push(transition_id.to_string());
            match fx.post {
                Post::Applied => {
                    fx.status = in_progress();
                    TransitionResult::Acknowledged {
                        observed_status: None,
                        verified: false,
                    }
                }
                Post::AcceptedStale => TransitionResult::Acknowledged {
                    observed_status: None,
                    verified: false,
                },
                Post::Rejected => TransitionResult::Rejected {
                    message: "Transition is not valid".into(),
                },
                Post::UnknownApplied => {
                    fx.status = in_progress();
                    TransitionResult::Unknown {
                        message: "HTTP 500 boom".into(),
                        observed_status: None,
                    }
                }
            }
        }
        fn add_comment(&self, _key: &str, _body: &str) -> CommentResult {
            unreachable!()
        }
    }

    struct Env {
        root: PathBuf,
        jira: Arc<FakeJira>,
        state: StateStore,
        service: TransitionService,
    }

    impl Drop for Env {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn env(post: Post) -> Env {
        let root = temp_dir("jira-transitions");
        let state = StateStore::new(root.join("state")).unwrap();
        let jira = Arc::new(FakeJira {
            fx: Mutex::new(Fixture {
                status: todo(),
                transitions: vec![start()],
                posts: vec![],
                post,
                record_at_post: vec![],
                after_transitions_get: None,
            }),
            record_dir: root.join("state").join("records").join("transitions"),
        });
        let service = TransitionService::new(jira.clone(), state.clone(), SITE.to_string());
        Env {
            root,
            jira,
            state,
            service,
        }
    }

    impl Env {
        fn record(&self) -> Option<TransitionRecord> {
            self.service.read(KEY).unwrap()
        }
        fn posts(&self) -> usize {
            self.jira.fx.lock().unwrap().posts.len()
        }
        fn approve(&self) -> (StatusRef, Transition) {
            let choices = self.service.load(KEY).unwrap();
            let t = choices
                .transitions
                .iter()
                .find(|t| t.id == "31")
                .unwrap()
                .clone();
            TransitionService::eligibility(&t).unwrap();
            (choices.issue.status, t)
        }
    }

    #[test]
    fn eligibility_refuses_unavailable_screen_and_required_fields() {
        let unavailable = Transition {
            is_available: Some(false),
            ..start()
        };
        let screen = Transition {
            has_screen: true,
            ..start()
        };
        let fields = Transition {
            requires_fields: true,
            ..start()
        };
        assert!(TransitionService::eligibility(&unavailable).is_err());
        assert!(TransitionService::eligibility(&screen).is_err());
        assert!(TransitionService::eligibility(&fields).is_err());
        assert!(
            TransitionService::eligibility(&Transition {
                is_available: None,
                ..start()
            })
            .is_ok()
        );

        let e = env(Post::Applied);
        assert!(matches!(
            e.service.apply(KEY, &todo(), &screen),
            Err(TransitionError::Ineligible(_))
        ));
        assert_eq!(e.posts(), 0);
    }

    #[test]
    fn changed_transition_list_discards_approval_without_post() {
        let e = env(Post::Applied);
        e.jira.fx.lock().unwrap().after_transitions_get =
            Some(Box::new(|list, _| list[0].to = status("5", "Resolved")));
        let (from, t) = e.approve();
        assert!(matches!(
            e.service.apply(KEY, &from, &t),
            Err(TransitionError::Changed(_))
        ));
        assert_eq!(e.posts(), 0);
        assert!(e.record().is_none());
    }

    #[test]
    fn changed_status_discards_approval_without_post() {
        let e = env(Post::Applied);
        let (from, t) = e.approve();
        e.jira.fx.lock().unwrap().status = status("4", "Reopened");
        assert!(matches!(
            e.service.apply(KEY, &from, &t),
            Err(TransitionError::Changed(_))
        ));
        assert_eq!(e.posts(), 0);
        assert!(e.record().is_none());
    }

    #[test]
    fn single_post_with_pending_record_first_and_verified_target() {
        let e = env(Post::Applied);
        let (from, t) = e.approve();
        let result = e.service.apply(KEY, &from, &t).unwrap();
        assert_eq!(
            result,
            TransitionResult::Acknowledged {
                observed_status: Some(in_progress()),
                verified: true
            }
        );
        let fx = e.jira.fx.lock().unwrap();
        assert_eq!(fx.posts, vec!["31".to_string()]);
        assert_eq!(fx.record_at_post, vec![true]);
        drop(fx);
        let record = e.record().unwrap();
        assert_eq!(record.state, RecordState::Acknowledged);
        assert_eq!(record.site, SITE);
        assert_eq!(record.key, KEY);
        assert_eq!(record.transition_id, "31");
        assert_eq!(record.from_status_id, "10000");
        assert_eq!(record.to_status_id, "3");
    }

    #[test]
    fn accepted_without_status_change_is_unverified() {
        let e = env(Post::AcceptedStale);
        let (from, t) = e.approve();
        let result = e.service.apply(KEY, &from, &t).unwrap();
        assert_eq!(
            result,
            TransitionResult::Acknowledged {
                observed_status: Some(todo()),
                verified: false
            }
        );
        assert_eq!(e.posts(), 1);
        assert_eq!(e.record().unwrap().state, RecordState::Acknowledged);
    }

    #[test]
    fn rejection_is_recorded() {
        let e = env(Post::Rejected);
        let (from, t) = e.approve();
        let result = e.service.apply(KEY, &from, &t).unwrap();
        assert!(
            matches!(result, TransitionResult::Rejected { message } if message.contains("not valid"))
        );
        assert_eq!(e.record().unwrap().state, RecordState::Rejected);
        // A definite rejection does not block the next attempt.
        assert!(e.service.load(KEY).unwrap().blocked.is_none());
    }

    #[test]
    fn unknown_blocks_next_apply_until_acknowledged() {
        let e = env(Post::UnknownApplied);
        let (from, t) = e.approve();
        let result = e.service.apply(KEY, &from, &t).unwrap();
        assert!(matches!(
            &result,
            TransitionResult::Unknown { observed_status: Some(s), .. } if *s == in_progress()
        ));
        assert_eq!(e.posts(), 1);
        assert_eq!(e.record().unwrap().state, RecordState::Unknown);

        // Back to To Do so the tuple would otherwise match.
        e.jira.fx.lock().unwrap().status = todo();
        e.jira.fx.lock().unwrap().post = Post::Applied;
        let blocked = e.service.load(KEY).unwrap().blocked.unwrap();
        assert!(matches!(
            e.service.apply(KEY, &from, &t),
            Err(TransitionError::Blocked { .. })
        ));
        assert_eq!(e.posts(), 1);

        e.service
            .acknowledge_unknown(KEY, &blocked.attempt)
            .unwrap();
        assert!(e.service.load(KEY).unwrap().blocked.is_none());
        e.service.apply(KEY, &from, &t).unwrap();
        assert_eq!(e.posts(), 2);
        assert_eq!(e.record().unwrap().state, RecordState::Acknowledged);
    }

    #[test]
    fn pending_record_from_previous_process_blocks() {
        let e = env(Post::Applied);
        let previous = TransitionRecord {
            version: 1,
            attempt: "previous-attempt".into(),
            key: KEY.into(),
            site: SITE.into(),
            transition_id: "31".into(),
            from_status_id: "10000".into(),
            to_status_id: "3".into(),
            state: RecordState::Pending,
            started_at: "2026-10-12T00:00:00.000Z".into(),
        };
        e.state
            .write_record(&e.service.record_path(KEY), &previous)
            .unwrap();
        let (from, t) = e.approve();
        assert_eq!(
            e.service.load(KEY).unwrap().blocked,
            Some(BlockedAttempt {
                attempt: "previous-attempt".into(),
                started_at: "2026-10-12T00:00:00.000Z".into(),
            })
        );
        assert!(matches!(
            e.service.apply(KEY, &from, &t),
            Err(TransitionError::Blocked { .. })
        ));
        assert_eq!(e.posts(), 0);
        assert_eq!(e.record().unwrap().attempt, "previous-attempt");

        e.service
            .acknowledge_unknown(KEY, "previous-attempt")
            .unwrap();
        e.service.apply(KEY, &from, &t).unwrap();
        assert_eq!(e.posts(), 1);
        let record = e.record().unwrap();
        assert_eq!(record.state, RecordState::Acknowledged);
        assert_ne!(record.attempt, "previous-attempt");
    }

    #[test]
    fn stale_acknowledgement_never_inspects_a_newer_attempt() {
        let e = env(Post::UnknownApplied);
        let (from, t) = e.approve();
        e.service.apply(KEY, &from, &t).unwrap();
        assert_eq!(e.posts(), 1);
        let seen = e.service.load(KEY).unwrap().blocked.unwrap();

        // A newer request (attempt Y) is recorded after the user saw attempt X.
        let newer = TransitionRecord {
            attempt: "attempt-y".into(),
            state: RecordState::Pending,
            started_at: "2026-10-13T00:00:00.000Z".into(),
            ..e.record().unwrap()
        };
        assert_ne!(newer.attempt, seen.attempt);
        e.state
            .write_record(&e.service.record_path(KEY), &newer)
            .unwrap();

        let err = e
            .service
            .acknowledge_unknown(KEY, &seen.attempt)
            .unwrap_err();
        assert!(matches!(
            &err,
            TransitionError::Blocked { attempt, record_started_at }
                if attempt == "attempt-y" && record_started_at == "2026-10-13T00:00:00.000Z"
        ));
        let stored = e.record().unwrap();
        assert_eq!(stored.attempt, "attempt-y");
        assert_eq!(stored.state, RecordState::Pending);

        e.jira.fx.lock().unwrap().status = todo();
        e.jira.fx.lock().unwrap().post = Post::Applied;
        assert!(matches!(
            e.service.apply(KEY, &from, &t),
            Err(TransitionError::Blocked { .. })
        ));
        assert_eq!(e.posts(), 1);

        e.service.acknowledge_unknown(KEY, "attempt-y").unwrap();
        assert_eq!(e.record().unwrap().state, RecordState::Inspected);
    }

    #[test]
    fn acknowledgement_without_uncertainty_does_not_write() {
        let e = env(Post::Applied);
        e.service.acknowledge_unknown(KEY, "any").unwrap();
        assert!(e.record().is_none());

        for state in [
            RecordState::Inspected,
            RecordState::Acknowledged,
            RecordState::Rejected,
        ] {
            let record = TransitionRecord {
                version: 1,
                attempt: "settled".into(),
                key: KEY.into(),
                site: SITE.into(),
                transition_id: "31".into(),
                from_status_id: "10000".into(),
                to_status_id: "3".into(),
                state,
                started_at: "2026-10-12T00:00:00.000Z".into(),
            };
            e.state
                .write_record(&e.service.record_path(KEY), &record)
                .unwrap();
            e.service.acknowledge_unknown(KEY, "other").unwrap();
            let stored = e.record().unwrap();
            assert_eq!(stored.state, state);
            assert_eq!(stored.attempt, "settled");
        }
    }

    #[test]
    fn held_lock_reports_busy_without_post() {
        let e = env(Post::Applied);
        let (from, t) = e.approve();
        let lock = e.service.lock_key(KEY);
        let inner = e
            .state
            .with_locks(&[lock], || e.service.apply(KEY, &from, &t))
            .unwrap();
        assert!(matches!(inner, Err(TransitionError::Lock(_))));
        assert_eq!(e.posts(), 0);
        assert!(e.record().is_none());
    }
}
