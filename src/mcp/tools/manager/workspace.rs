use super::{
    append_action_log, load_session, parse_params, parse_scope, save_session, sort_notes, to_value,
};
use crate::mcp::dispatcher::{Dispatcher, Tool};
use crate::mcp::error::McpError;
use crate::mcp::push::PushRegistry;
use crate::mcp::server::ConnectedClient;
use crate::storage::Storage;
use crate::types::{JobSummary, WorkerRole, WorkerSummary, WorkspaceNote, WorkspaceStatus};
use async_trait::async_trait;
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::RwLock;

pub fn register(
    dispatcher: &mut Dispatcher,
    storage: Arc<Storage>,
    push: Arc<RwLock<PushRegistry>>,
) {
    dispatcher.register(Box::new(WorkspaceStatusTool::new(
        Arc::clone(&storage),
        Arc::clone(&push),
    )));
    dispatcher.register(Box::new(WorkspaceLogTool::new(
        Arc::clone(&storage),
        Arc::clone(&push),
    )));
    dispatcher.register(Box::new(WorkspaceNoteTool::new(
        Arc::clone(&storage),
        Arc::clone(&push),
    )));
    dispatcher.register(Box::new(WorkspaceNotesTool::new(storage, push)));
}

pub struct WorkspaceStatusTool {
    storage: Arc<Storage>,
    push: Arc<RwLock<PushRegistry>>,
}

impl WorkspaceStatusTool {
    pub fn new(storage: Arc<Storage>, push: Arc<RwLock<PushRegistry>>) -> Self {
        Self { storage, push }
    }
}

#[async_trait]
impl Tool for WorkspaceStatusTool {
    fn name(&self) -> &str {
        "workspace.status"
    }

    fn allowed_roles(&self) -> &[WorkerRole] {
        &[WorkerRole::Manager, WorkerRole::Worker]
    }

    async fn call(&self, _params: Value, _caller: &ConnectedClient) -> Result<Value, McpError> {
        let _ = &self.push;
        let session = load_session(&self.storage)?;
        let manager = session
            .workers
            .values()
            .find(|worker| worker.role == WorkerRole::Manager)
            .map(|worker| WorkerSummary {
                id: worker.id.clone(),
                provider: worker.provider.clone(),
                status: worker.status.clone(),
                job_id: worker.job_id.clone(),
                context_pct: worker.context_usage_pct,
            });
        let mut workers = session
            .workers
            .values()
            .filter(|worker| worker.role == WorkerRole::Worker)
            .map(|worker| WorkerSummary {
                id: worker.id.clone(),
                provider: worker.provider.clone(),
                status: worker.status.clone(),
                job_id: worker.job_id.clone(),
                context_pct: worker.context_usage_pct,
            })
            .collect::<Vec<_>>();
        workers.sort_by(|a, b| a.id.cmp(&b.id));

        let mut jobs = session
            .jobs
            .values()
            .map(|job| JobSummary {
                id: job.id.clone(),
                intent: job.intent.clone(),
                status: job.status.clone(),
                worker_id: job.worker_id.clone(),
                depends_on: job.depends_on.clone(),
                created_at: job.created_at,
            })
            .collect::<Vec<_>>();
        jobs.sort_by(|a, b| a.id.cmp(&b.id));

        to_value(&WorkspaceStatus {
            session_id: session.id,
            manager,
            workers,
            jobs,
            notes: session.notes,
        })
    }
}

#[derive(Deserialize)]
struct WorkspaceLogParams {
    limit: Option<u32>,
}

pub struct WorkspaceLogTool {
    storage: Arc<Storage>,
    push: Arc<RwLock<PushRegistry>>,
}

impl WorkspaceLogTool {
    pub fn new(storage: Arc<Storage>, push: Arc<RwLock<PushRegistry>>) -> Self {
        Self { storage, push }
    }
}

#[async_trait]
impl Tool for WorkspaceLogTool {
    fn name(&self) -> &str {
        "workspace.log"
    }

    fn allowed_roles(&self) -> &[WorkerRole] {
        &[WorkerRole::Manager]
    }

    async fn call(&self, params: Value, _caller: &ConnectedClient) -> Result<Value, McpError> {
        let _ = &self.push;
        let params = parse_params::<WorkspaceLogParams>(params)?;
        let entries = self
            .storage
            .read_action_log(Some(params.limit.unwrap_or(50) as usize))
            .map_err(super::storage_error)?;
        to_value(&entries)
    }
}

#[derive(Deserialize)]
struct WorkspaceNoteParams {
    content: String,
    scope: Option<String>,
}

pub struct WorkspaceNoteTool {
    storage: Arc<Storage>,
    push: Arc<RwLock<PushRegistry>>,
}

impl WorkspaceNoteTool {
    pub fn new(storage: Arc<Storage>, push: Arc<RwLock<PushRegistry>>) -> Self {
        Self { storage, push }
    }
}

#[async_trait]
impl Tool for WorkspaceNoteTool {
    fn name(&self) -> &str {
        "workspace.note"
    }

    fn allowed_roles(&self) -> &[WorkerRole] {
        &[WorkerRole::Manager]
    }

    async fn call(&self, params: Value, caller: &ConnectedClient) -> Result<Value, McpError> {
        let _ = &self.push;
        let params = parse_params::<WorkspaceNoteParams>(params.clone())?;
        let mut session = load_session(&self.storage)?;
        let note_id = format!("note_{}", Utc::now().format("%Y%m%dT%H%M%S%3f"));
        session.notes.push(WorkspaceNote {
            id: note_id.clone(),
            content: params.content,
            scope: parse_scope(params.scope),
            created_at: Utc::now(),
        });
        save_session(&self.storage, &session)?;
        append_action_log(
            &self.storage,
            caller,
            self.name(),
            json!({
                "content": session.notes.last().map(|note| note.content.clone()).unwrap_or_default(),
                "scope": session.notes.last().map(|note| match &note.scope {
                    crate::types::NoteScope::Global => "global".to_string(),
                    crate::types::NoteScope::Job(job_id) => format!("job:{job_id}"),
                    crate::types::NoteScope::Directory(dir) => dir.clone(),
                }).unwrap_or_else(|| "global".to_string())
            }),
            Some(json!({ "note_id": note_id })),
        )?;
        Ok(Value::String(note_id))
    }
}

pub struct WorkspaceNotesTool {
    storage: Arc<Storage>,
    push: Arc<RwLock<PushRegistry>>,
}

impl WorkspaceNotesTool {
    pub fn new(storage: Arc<Storage>, push: Arc<RwLock<PushRegistry>>) -> Self {
        Self { storage, push }
    }
}

#[async_trait]
impl Tool for WorkspaceNotesTool {
    fn name(&self) -> &str {
        "workspace.notes"
    }

    fn allowed_roles(&self) -> &[WorkerRole] {
        &[WorkerRole::Manager]
    }

    async fn call(&self, _params: Value, _caller: &ConnectedClient) -> Result<Value, McpError> {
        let _ = &self.push;
        let mut notes = load_session(&self.storage)?.notes;
        sort_notes(&mut notes);
        to_value(&notes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::dispatcher::Tool;
    use crate::mcp::tools::manager::testsupport::{call_tool, setup, worker};
    use crate::types::{Job, JobStatus, NoteScope, WorkerRole, WorkerStatus};
    use serde_json::json;

    #[tokio::test]
    async fn workspace_note_persists_and_notes_are_sorted() {
        let (_temp, storage, push, caller) = setup();
        let note_tool = WorkspaceNoteTool::new(Arc::clone(&storage), Arc::clone(&push));
        let notes_tool = WorkspaceNotesTool::new(Arc::clone(&storage), Arc::clone(&push));

        let job_note_id = call_tool(
            &note_tool,
            json!({"content":"job note","scope":"job:job_001"}),
            &caller,
        )
        .await
        .unwrap();
        assert!(job_note_id.as_str().unwrap().starts_with("note_"));

        call_tool(
            &note_tool,
            json!({"content":"global note","scope":"global"}),
            &caller,
        )
        .await
        .unwrap();
        call_tool(
            &note_tool,
            json!({"content":"dir note","scope":"src/mcp"}),
            &caller,
        )
        .await
        .unwrap();

        let stored = storage.load_session().unwrap().unwrap();
        assert_eq!(stored.notes.len(), 3);

        let notes = notes_tool.call(Value::Null, &caller).await.unwrap();
        let notes: Vec<WorkspaceNote> = serde_json::from_value(notes).unwrap();
        assert!(matches!(notes[0].scope, NoteScope::Job(_)));
        assert!(matches!(notes[1].scope, NoteScope::Directory(_)));
        assert!(matches!(notes[2].scope, NoteScope::Global));
    }

    #[tokio::test]
    async fn workspace_status_returns_manager_workers_and_jobs() {
        let (_temp, storage, push, caller) = setup();
        let mut session = storage.load_session().unwrap().unwrap();
        session
            .workers
            .insert("w1".to_string(), worker("w1", WorkerStatus::Idle));
        session.jobs.insert(
            "job_001".to_string(),
            Job {
                id: "job_001".to_string(),
                intent: "Implement feature".to_string(),
                status: JobStatus::Pending,
                worker_id: None,
                depends_on: vec![],
                created_at: Utc::now(),
                updated_at: Utc::now(),
                branch: None,
                branch_start_commit: None,
                checkpoints: vec![],
                result: None,
                fail_count: 0,
                last_fail_at: None,
            },
        );
        storage.save_session(&session).unwrap();

        let tool = WorkspaceStatusTool::new(storage, push);
        let value = tool.call(Value::Null, &caller).await.unwrap();
        let status: WorkspaceStatus = serde_json::from_value(value).unwrap();

        assert_eq!(status.manager.as_ref().map(|m| m.id.as_str()), Some("wm"));
        assert_eq!(status.workers.len(), 1);
        assert_eq!(status.jobs.len(), 1);
        assert_eq!(status.workers[0].id, "w1");
        assert_eq!(status.jobs[0].id, "job_001");
        assert_eq!(caller.role, WorkerRole::Manager);
    }
}
