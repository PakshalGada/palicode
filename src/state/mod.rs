//! Durable state: SQLite schema, sessions, checkpoints, plans/tasks, handoffs.
//!
//! The database uses WAL with `synchronous=FULL`, and each checkpoint is a
//! transactional snapshot of the conversation. This survives a kill -9: the
//! last committed snapshot is always readable.

use crate::agent::plan::{Plan, StepStatus};
use crate::provider::types::Role;
use crate::provider::{Message, ToolCall};
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::debug;

pub const SESSION_ACTIVE: &str = "active";
pub const SESSION_IDLE: &str = "idle";
pub const SESSION_INTERRUPTED: &str = "interrupted";

#[derive(Debug, Clone)]
pub struct SessionRecord {
    pub id: String,
    pub title: String,
    pub provider: String,
    pub model: String,
    pub cwd: String,
    pub status: String,
    pub turns: usize,
    pub created_at: i64,
    pub updated_at: i64,
}

impl SessionRecord {
    pub fn interrupted(&self) -> bool {
        self.status == SESSION_INTERRUPTED
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    pub id: String,
    pub session_id: String,
    pub index: usize,
    pub title: String,
    pub description: String,
    pub status: String,
    pub depends_on: Vec<String>,
    pub result: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Thread-safe handle to the SQLite database. Cheap to clone.
#[derive(Clone)]
pub struct StateStore {
    connection: Arc<Mutex<Connection>>,
    path: PathBuf,
}

impl StateStore {
    pub fn open() -> Result<Self> {
        Self::open_at(&default_path()?)
    }

    pub fn open_at(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
            }
        }

        let mut connection = Connection::open(path)
            .with_context(|| format!("cannot open state database {}", path.display()))?;
        connection
            .query_row("PRAGMA journal_mode=WAL", [], |row| row.get::<_, String>(0))
            .context("cannot enable WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.busy_timeout(Duration::from_secs(5))?;
        migrate(&mut connection)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
            // WAL/SHM files contain conversation data too.
            for suffix in ["-wal", "-shm"] {
                let sidecar = PathBuf::from(format!("{}{suffix}", path.display()));
                if sidecar.exists() {
                    let _ =
                        std::fs::set_permissions(sidecar, std::fs::Permissions::from_mode(0o600));
                }
            }
        }

        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>> {
        self.connection
            .lock()
            .map_err(|_| anyhow::anyhow!("state database lock poisoned"))
    }

    pub fn insert_session(&self, session: &SessionRecord) -> Result<()> {
        let connection = self.lock()?;
        connection.execute(
            "INSERT INTO sessions
                (id, title, provider, model, cwd, status, turns, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(id) DO UPDATE SET
                title=excluded.title,
                provider=excluded.provider,
                model=excluded.model,
                cwd=excluded.cwd,
                status=excluded.status,
                turns=excluded.turns,
                updated_at=excluded.updated_at",
            params![
                session.id,
                session.title,
                session.provider,
                session.model,
                session.cwd,
                session.status,
                session.turns as i64,
                session.created_at,
                session.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn update_session_meta(&self, id: &str, title: &str, turns: usize) -> Result<()> {
        let connection = self.lock()?;
        connection.execute(
            "UPDATE sessions SET title=?2, turns=?3, updated_at=?4 WHERE id=?1",
            params![id, title, turns as i64, now_epoch()],
        )?;
        Ok(())
    }

    pub fn set_status(&self, id: &str, status: &str) -> Result<()> {
        let connection = self.lock()?;
        connection.execute(
            "UPDATE sessions SET status=?2, updated_at=?3 WHERE id=?1",
            params![id, status, now_epoch()],
        )?;
        Ok(())
    }

    pub fn set_active_session(&self, id: &str) -> Result<()> {
        {
            let connection = self.lock()?;
            connection.execute(
                "UPDATE sessions SET status=?1
                 WHERE status=?2 AND cwd=(SELECT cwd FROM sessions WHERE id=?3)",
                params![SESSION_IDLE, SESSION_ACTIVE, id],
            )?;
        }
        self.set_status(id, SESSION_ACTIVE)
    }

    pub fn list_sessions_for_cwd(&self, cwd: &str) -> Result<Vec<SessionRecord>> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT id, title, provider, model, cwd, status, turns, created_at, updated_at
             FROM sessions WHERE cwd=?1 ORDER BY updated_at DESC",
        )?;
        let rows = statement.query_map(params![cwd], map_session_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("cannot read sessions")
    }

    pub fn get_session(&self, id: &str) -> Result<Option<SessionRecord>> {
        let connection = self.lock()?;
        connection
            .query_row(
                "SELECT id, title, provider, model, cwd, status, turns, created_at, updated_at
                 FROM sessions WHERE id=?1",
                params![id],
                |row| {
                    Ok(SessionRecord {
                        id: row.get(0)?,
                        title: row.get(1)?,
                        provider: row.get(2)?,
                        model: row.get(3)?,
                        cwd: row.get(4)?,
                        status: row.get(5)?,
                        turns: row.get::<_, i64>(6)? as usize,
                        created_at: row.get(7)?,
                        updated_at: row.get(8)?,
                    })
                },
            )
            .optional()
            .context("cannot read session")
    }

    /// Mark sessions left `active` by a crash in this project as interrupted.
    pub fn mark_active_interrupted_for_cwd(&self, cwd: &str) -> Result<Vec<SessionRecord>> {
        {
            let connection = self.lock()?;
            connection.execute(
                "UPDATE sessions SET status=?1 WHERE status=?2 AND cwd=?3",
                params![SESSION_INTERRUPTED, SESSION_ACTIVE, cwd],
            )?;
        }
        Ok(self
            .list_sessions_for_cwd(cwd)?
            .into_iter()
            .filter(SessionRecord::interrupted)
            .collect())
    }

    pub fn delete_session(&self, id: &str) -> Result<()> {
        let connection = self.lock()?;
        connection.execute("DELETE FROM sessions WHERE id=?1", params![id])?;
        Ok(())
    }

    /// Transactional full-snapshot checkpoint.
    pub fn save_checkpoint(
        &self,
        session_id: &str,
        messages: &[Message],
        kind: &str,
    ) -> Result<()> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let now = now_epoch();
        transaction.execute(
            "INSERT OR IGNORE INTO sessions
                (id, title, provider, model, cwd, status, turns, created_at, updated_at)
             VALUES (?1, ?2, '', '', '', ?3, 0, ?4, ?4)",
            params![session_id, "Session", SESSION_IDLE, now],
        )?;
        transaction.execute(
            "DELETE FROM messages WHERE session_id=?1",
            params![session_id],
        )?;
        for (seq, message) in messages.iter().enumerate() {
            let tool_calls =
                serde_json::to_string(&message.tool_calls).context("cannot encode tool calls")?;
            transaction.execute(
                "INSERT INTO messages
                    (session_id, seq, role, content, tool_calls, tool_call_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    session_id,
                    seq as i64,
                    message.role.as_str(),
                    message.content,
                    tool_calls,
                    message.tool_call_id,
                ],
            )?;
        }
        transaction.execute(
            "INSERT INTO checkpoints (session_id, kind, created_at) VALUES (?1, ?2, ?3)",
            params![session_id, kind, now],
        )?;
        transaction.execute(
            "UPDATE sessions SET updated_at=?2 WHERE id=?1",
            params![session_id, now],
        )?;
        transaction.execute(
            "DELETE FROM checkpoints WHERE session_id=?1 AND id NOT IN (
                SELECT id FROM checkpoints WHERE session_id=?1 ORDER BY id DESC LIMIT 50
             )",
            params![session_id],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn load_messages(&self, session_id: &str) -> Result<Vec<Message>> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT role, content, tool_calls, tool_call_id
             FROM messages WHERE session_id=?1 ORDER BY seq",
        )?;
        let rows = statement.query_map(params![session_id], |row| {
            let role: String = row.get(0)?;
            let content: Option<String> = row.get(1)?;
            let tool_calls: String = row.get(2)?;
            let tool_call_id: Option<String> = row.get(3)?;
            Ok((role, content, tool_calls, tool_call_id))
        })?;

        let mut messages = Vec::new();
        for row in rows {
            let (role, content, tool_calls, tool_call_id) = row?;
            let role = match role.as_str() {
                "system" => Role::System,
                "user" => Role::User,
                "assistant" => Role::Assistant,
                "tool" => Role::Tool,
                _ => Role::User,
            };
            let tool_calls: Vec<ToolCall> = serde_json::from_str(&tool_calls).unwrap_or_default();
            messages.push(Message {
                role,
                content,
                tool_calls,
                tool_call_id,
            });
        }
        Ok(messages)
    }

    pub fn save_plan(&self, session_id: &str, plan: &Plan) -> Result<()> {
        let plan_json = serde_json::to_string(plan).context("cannot encode plan")?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let now = now_epoch();
        transaction.execute(
            "INSERT INTO plans (session_id, plan_json, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(session_id) DO UPDATE SET plan_json=excluded.plan_json,
                                                   updated_at=excluded.updated_at",
            params![session_id, plan_json, now],
        )?;
        transaction.execute("DELETE FROM tasks WHERE session_id=?1", params![session_id])?;
        for (index, step) in plan.steps.iter().enumerate() {
            let id = task_id(session_id, index);
            let depends_on = if index == 0 {
                Vec::new()
            } else {
                vec![task_id(session_id, index - 1)]
            };
            transaction.execute(
                "INSERT INTO tasks
                    (id, session_id, idx, title, description, status, depends_on, result, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?4, ?5, ?6, NULL, ?7, ?7)",
                params![
                    id,
                    session_id,
                    index as i64,
                    step.description,
                    step_status(step.status),
                    serde_json::to_string(&depends_on)?,
                    now,
                ],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn clear_plan(&self, session_id: &str) -> Result<()> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        transaction.execute("DELETE FROM plans WHERE session_id=?1", params![session_id])?;
        transaction.execute("DELETE FROM tasks WHERE session_id=?1", params![session_id])?;
        transaction.commit()?;
        Ok(())
    }

    pub fn load_plan(&self, session_id: &str) -> Result<Option<Plan>> {
        let connection = self.lock()?;
        let plan_json: Option<String> = connection
            .query_row(
                "SELECT plan_json FROM plans WHERE session_id=?1",
                params![session_id],
                |row| row.get(0),
            )
            .optional()?;
        match plan_json {
            Some(json) => Ok(Some(serde_json::from_str(&json)?)),
            None => Ok(None),
        }
    }

    pub fn list_tasks(&self, session_id: &str) -> Result<Vec<TaskRecord>> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT id, session_id, idx, title, description, status, depends_on, result,
                    created_at, updated_at
             FROM tasks WHERE session_id=?1 ORDER BY idx",
        )?;
        let rows = statement.query_map(params![session_id], |row| {
            let depends_on: String = row.get(6)?;
            Ok(TaskRecord {
                id: row.get(0)?,
                session_id: row.get(1)?,
                index: row.get::<_, i64>(2)? as usize,
                title: row.get(3)?,
                description: row.get(4)?,
                status: row.get(5)?,
                depends_on: serde_json::from_str(&depends_on).unwrap_or_default(),
                result: row.get(7)?,
                created_at: row.get(8)?,
                updated_at: row.get(9)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("cannot read tasks")
    }

    pub fn update_task_status(
        &self,
        session_id: &str,
        index: usize,
        status: StepStatus,
    ) -> Result<()> {
        let connection = self.lock()?;
        connection.execute(
            "UPDATE tasks SET status=?3, updated_at=?4 WHERE id=?1 AND session_id=?2",
            params![
                task_id(session_id, index),
                session_id,
                step_status(status),
                now_epoch()
            ],
        )?;
        Ok(())
    }

    pub fn save_handoff(&self, session_id: &str, path: &str, summary: &str) -> Result<()> {
        let connection = self.lock()?;
        connection.execute(
            "INSERT INTO handoffs (session_id, path, summary, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![session_id, path, summary, now_epoch()],
        )?;
        Ok(())
    }
}

fn map_session_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRecord> {
    Ok(SessionRecord {
        id: row.get(0)?,
        title: row.get(1)?,
        provider: row.get(2)?,
        model: row.get(3)?,
        cwd: row.get(4)?,
        status: row.get(5)?,
        turns: row.get::<_, i64>(6)? as usize,
        created_at: row.get(7)?,
        updated_at: row.get(8)?,
    })
}

fn migrate(connection: &mut Connection) -> Result<()> {
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < 1 {
        connection.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                provider TEXT NOT NULL DEFAULT '',
                model TEXT NOT NULL DEFAULT '',
                cwd TEXT NOT NULL DEFAULT '',
                status TEXT NOT NULL DEFAULT 'idle',
                turns INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                seq INTEGER NOT NULL,
                role TEXT NOT NULL,
                content TEXT,
                tool_calls TEXT NOT NULL DEFAULT '[]',
                tool_call_id TEXT,
                UNIQUE(session_id, seq)
            );

            CREATE TABLE IF NOT EXISTS checkpoints (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                kind TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS plans (
                session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
                plan_json TEXT NOT NULL,
                updated_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS tasks (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                idx INTEGER NOT NULL,
                title TEXT NOT NULL,
                description TEXT,
                status TEXT NOT NULL,
                depends_on TEXT NOT NULL DEFAULT '[]',
                result TEXT,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS handoffs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                path TEXT NOT NULL,
                summary TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id, seq);
            CREATE INDEX IF NOT EXISTS idx_checkpoints_session ON checkpoints(session_id, id);
            CREATE INDEX IF NOT EXISTS idx_tasks_session ON tasks(session_id, idx);
            PRAGMA user_version = 1;
            "#,
        )?;
        debug!(version = 1, "applied state database migration");
    }
    Ok(())
}

fn task_id(session_id: &str, index: usize) -> String {
    format!("{session_id}:{index}")
}

fn step_status(status: StepStatus) -> &'static str {
    match status {
        StepStatus::Pending => "pending",
        StepStatus::Running => "running",
        StepStatus::Done => "done",
        StepStatus::Failed => "failed",
        StepStatus::Skipped => "skipped",
    }
}

pub fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

pub fn default_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("PALICODE_DB")
        && !path.is_empty()
    {
        return Ok(PathBuf::from(path));
    }
    let base = dirs::data_dir().context("cannot determine data directory")?;
    Ok(base.join("palicode").join("palicode.db"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::plan::{Plan, PlanStep, Risk};
    use tempfile::tempdir;

    fn store() -> (StateStore, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let store = StateStore::open_at(&dir.path().join("test.db")).unwrap();
        (store, dir)
    }

    fn session_record(id: &str, status: &str) -> SessionRecord {
        SessionRecord {
            id: id.to_string(),
            title: "Test session".to_string(),
            provider: "ollama".to_string(),
            model: "qwen3.5:9b".to_string(),
            cwd: "/tmp/work".to_string(),
            status: status.to_string(),
            turns: 2,
            created_at: now_epoch(),
            updated_at: now_epoch(),
        }
    }

    #[test]
    fn sessions_round_trip_and_crash_marking() {
        let (store, _dir) = store();
        store
            .insert_session(&session_record("a", SESSION_ACTIVE))
            .unwrap();
        store
            .insert_session(&session_record("b", SESSION_IDLE))
            .unwrap();
        let sessions = store.list_sessions_for_cwd("/tmp/work").unwrap();
        assert_eq!(sessions.len(), 2);

        let interrupted = store.mark_active_interrupted_for_cwd("/tmp/work").unwrap();
        assert_eq!(interrupted.len(), 1);
        assert_eq!(interrupted[0].id, "a");
        assert_eq!(
            store.get_session("a").unwrap().unwrap().status,
            SESSION_INTERRUPTED
        );
        assert_eq!(
            store.get_session("b").unwrap().unwrap().status,
            SESSION_IDLE
        );
    }

    #[test]
    fn sessions_are_scoped_to_a_project() {
        let (store, _dir) = store();
        let mut first = session_record("a", SESSION_ACTIVE);
        first.cwd = "/project/one".to_string();
        let mut second = session_record("b", SESSION_ACTIVE);
        second.cwd = "/project/two".to_string();
        store.insert_session(&first).unwrap();
        store.insert_session(&second).unwrap();

        let one = store.list_sessions_for_cwd("/project/one").unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].id, "a");

        let interrupted = store
            .mark_active_interrupted_for_cwd("/project/one")
            .unwrap();
        assert_eq!(interrupted.len(), 1);
        assert_eq!(interrupted[0].id, "a");
        assert_eq!(
            store.get_session("b").unwrap().unwrap().status,
            SESSION_ACTIVE
        );
        let two = store.list_sessions_for_cwd("/project/two").unwrap();
        assert_eq!(two[0].status, SESSION_ACTIVE);
    }

    #[test]
    fn checkpoints_round_trip_messages() {
        let (store, _dir) = store();
        store
            .insert_session(&session_record("s", SESSION_ACTIVE))
            .unwrap();
        let messages = vec![
            Message::system("sys"),
            Message::user("hello"),
            Message::assistant(
                Some("calling".to_string()),
                vec![ToolCall {
                    id: "call_1".to_string(),
                    name: "read_file".to_string(),
                    arguments: "{\"path\":\"a\"}".to_string(),
                }],
            ),
            Message::tool_result("call_1", "contents"),
        ];
        store
            .save_checkpoint("s", &messages, "tool_result")
            .unwrap();
        let loaded = store.load_messages("s").unwrap();
        assert_eq!(loaded.len(), 4);
        assert_eq!(loaded[1].content.as_deref(), Some("hello"));
        assert_eq!(loaded[2].tool_calls[0].name, "read_file");
        assert_eq!(loaded[3].tool_call_id.as_deref(), Some("call_1"));

        store
            .save_checkpoint("s", &messages[..2], "turn_end")
            .unwrap();
        assert_eq!(store.load_messages("s").unwrap().len(), 2);
    }

    #[test]
    fn plans_and_tasks_round_trip() {
        let (store, _dir) = store();
        store
            .insert_session(&session_record("s", SESSION_ACTIVE))
            .unwrap();
        let plan = Plan {
            title: "Plan".to_string(),
            goal: "Do work".to_string(),
            steps: vec![
                PlanStep {
                    description: "First".to_string(),
                    files: vec!["a.rs".to_string()],
                    risk: Risk::Low,
                    status: StepStatus::Pending,
                },
                PlanStep {
                    description: "Second".to_string(),
                    files: Vec::new(),
                    risk: Risk::Medium,
                    status: StepStatus::Pending,
                },
            ],
            notes: None,
        };
        store.save_plan("s", &plan).unwrap();
        let loaded = store.load_plan("s").unwrap().unwrap();
        assert_eq!(loaded.steps.len(), 2);
        let tasks = store.list_tasks("s").unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[1].depends_on, vec!["s:0".to_string()]);

        store.update_task_status("s", 0, StepStatus::Done).unwrap();
        assert_eq!(store.list_tasks("s").unwrap()[0].status, "done");
    }

    #[test]
    fn deleting_a_session_cascades() {
        let (store, _dir) = store();
        store
            .insert_session(&session_record("s", SESSION_ACTIVE))
            .unwrap();
        store
            .save_checkpoint("s", &[Message::user("hi")], "turn_end")
            .unwrap();
        store.delete_session("s").unwrap();
        assert!(store.get_session("s").unwrap().is_none());
        assert!(store.load_messages("s").unwrap().is_empty());
    }

    #[test]
    fn database_uses_wal() {
        let (store, _dir) = store();
        let connection = store.lock().unwrap();
        let mode: String = connection
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
    }
}
