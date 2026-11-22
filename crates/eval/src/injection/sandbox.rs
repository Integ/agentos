//! One case's AgentOS: the real butler, policy engine, audit log and task
//! store, on the in-memory world of [`super::world`].
//!
//! What a case writes (its audit log) lives in its own directory, which is
//! removed with the sandbox. The PIN vault is shared by every sandbox of an
//! evaluation and lives with the evaluation's scratch files.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use agentos_audit::{AuditEvent, AuditLog};
use agentos_core::{
    now_ms, AgentId, CapabilityName, CapabilitySpec, DeviceId, Intent, IntentId, Urgency,
};
use agentos_planner::{Conversation, Plan, Planner};
use agentos_policy::PolicyEngine;
use agentos_runtime::{Task, TaskId};
use agentos_services::agents::AgentDirectory;
use agentos_services::blackboard::Blackboard;
use agentos_services::expression::ExpressionHub;
use agentos_services::identity::PinVault;
use agentos_services::judge::Judge;
use agentos_taskd::butler::{grant_manifest, Budget, Butler};
use agentos_taskd::preauth::PreauthStore;
use async_trait::async_trait;

use super::world::{self, Planted, Protection, Source, World};

/// The PIN of an evaluation's own throwaway vault. It guards nothing: the
/// vault lives in the scratch directory and every capability is a double.
pub const PIN: &str = "246813";

/// A PIN vault shared by many sandboxes. Deriving a PIN hash is slow on
/// purpose, so one evaluation sets it once.
///
/// Every sandbox numbers its tasks from 1, and a PIN entry vouches for
/// "task N", so an irreversible approval enters the PIN and approves while
/// holding the gate: no other sandbox can use the entry in between.
pub struct SharedPin {
    vault: Arc<PinVault>,
    pub(crate) gate: tokio::sync::Mutex<()>,
}

impl SharedPin {
    pub fn create(dir: &Path) -> io::Result<Self> {
        agentos_runtime::create_private_dir(dir)?;
        let vault = Arc::new(PinVault::open(dir.join("pin.json"))?);
        vault.set(PIN)?;
        Ok(Self {
            vault,
            gate: tokio::sync::Mutex::new(()),
        })
    }

    pub(crate) fn vouch(&self, task: TaskId) {
        self.vault.verify_for(Some(task.0), PIN);
    }
}

/// What every sandbox of one evaluation shares.
pub struct Setup {
    pub planner: Arc<dyn Planner>,
    pub judge: Option<Arc<dyn Judge>>,
    pub agents: AgentDirectory,
    pub preauths: Option<Arc<PreauthStore>>,
    /// Without a PIN, irreversible steps are refused outright instead of
    /// asking; with one they ask, and approving takes the PIN.
    pub pin: Option<Arc<SharedPin>>,
    pub budget: Budget,
    /// Where sandboxes put their scratch directories.
    pub scratch: PathBuf,
}

impl Setup {
    /// The built-in agents, no judge, no pre-authorizations, no PIN, the
    /// default budget.
    pub fn new(planner: Arc<dyn Planner>, scratch: PathBuf) -> Self {
        Self {
            planner,
            judge: None,
            agents: AgentDirectory::with_builtin(),
            preauths: None,
            pin: None,
            budget: Budget::default(),
            scratch,
        }
    }
}

/// One planner for every sandbox's butler, counting the requests that
/// failed. A model that cannot be reached plans nothing, and nothing planned
/// would otherwise read as an attack that did not get through.
struct Shared {
    planner: Arc<dyn Planner>,
    errors: Arc<AtomicUsize>,
}

impl Shared {
    fn count(&self, plan: Plan) -> Plan {
        if plan.error.is_some() {
            self.errors.fetch_add(1, Ordering::Relaxed);
        }
        plan
    }
}

#[async_trait]
impl Planner for Shared {
    async fn plan(&self, intent: &Intent, available: &[CapabilitySpec]) -> Plan {
        self.count(self.planner.plan(intent, available).await)
    }

    async fn plan_turn(&self, conversation: &Conversation<'_>) -> Plan {
        self.count(self.planner.plan_turn(conversation).await)
    }
}

/// An intent as the task service makes one for the first desktop user.
pub fn typed(text: &str) -> Intent {
    Intent {
        id: IntentId(0),
        source: DeviceId::new("unix:uid:1000"),
        speaker: Some("uid:1000".into()),
        text: text.into(),
        urgency: Urgency::Normal,
        at_ms: now_ms(),
    }
}

pub struct Sandbox {
    pub butler: Butler,
    pub world: World,
    pub(crate) pin: Option<Arc<SharedPin>>,
    planner_errors: Arc<AtomicUsize>,
    dir: PathBuf,
}

impl Sandbox {
    /// A fresh AgentOS whose `source` carries `planted`. The butler holds
    /// every capability, as the daemon grants it everything registered.
    pub fn open(
        setup: &Setup,
        source: Source,
        planted: &Planted,
        protection: Protection,
    ) -> io::Result<Self> {
        static SANDBOXES: AtomicU64 = AtomicU64::new(0);
        let dir = setup.scratch.join(format!(
            "sandbox-{}-{}",
            std::process::id(),
            SANDBOXES.fetch_add(1, Ordering::Relaxed)
        ));
        agentos_runtime::create_private_dir(&dir)?;
        let (registry, world) = world::build(source, planted, protection);
        let id = AgentId::new("butler");
        let mut policy = PolicyEngine::new();
        let names: Vec<CapabilityName> = registry.specs().into_iter().map(|s| s.name).collect();
        grant_manifest(&mut policy, &id, &names);
        let audit = Arc::new(AuditLog::open(dir.join("audit.jsonl"))?);
        let planner_errors = Arc::new(AtomicUsize::new(0));
        let mut butler = Butler::new(
            id,
            registry,
            policy,
            Box::new(Shared {
                planner: setup.planner.clone(),
                errors: planner_errors.clone(),
            }),
            audit.clone(),
            ExpressionHub::new(audit),
        )
        .with_agents(setup.agents.clone())
        .with_blackboard(world.board.clone())
        .with_budget(setup.budget);
        if let Some(judge) = &setup.judge {
            butler = butler.with_judge(judge.clone());
        }
        if let Some(preauths) = &setup.preauths {
            butler = butler.with_preauthorizations(preauths.clone());
        }
        if let Some(pin) = &setup.pin {
            butler = butler.with_pin_vault(pin.vault.clone());
        }
        Ok(Self {
            butler,
            world,
            pin: setup.pin.clone(),
            planner_errors,
            dir,
        })
    }

    /// Submit `text` as the user and run it until it ends or waits.
    pub async fn submit(&self, text: &str) -> io::Result<TaskId> {
        let task = self.butler.tasks().create(typed(text))?;
        self.butler.execute(task.id).await;
        Ok(task.id)
    }

    /// Submit `text` as the next request of the conversation `thread` and
    /// run it until it ends or waits.
    pub async fn submit_in(&self, thread: TaskId, text: &str) -> io::Result<TaskId> {
        let task = self.butler.tasks().create_in(typed(text), Some(thread))?;
        self.butler.execute(task.id).await;
        Ok(task.id)
    }

    pub fn task(&self, id: TaskId) -> Task {
        self.butler.tasks().get(id).expect("a task of this sandbox")
    }

    /// Planning requests that failed so far.
    pub fn planner_errors(&self) -> usize {
        self.planner_errors.load(Ordering::Relaxed)
    }

    pub fn board(&self) -> &Blackboard {
        &self.world.board
    }

    /// Approve the step `id` waits on, as the human who saw the prompt,
    /// entering the PIN when the step asks for it.
    pub async fn approve(&self, id: TaskId) {
        let task = self.task(id);
        let Some(confirmation) = task.confirmation.clone() else {
            return;
        };
        let approve = || {
            self.butler
                .approve_with_token(id, confirmation.step, &task.intent, &confirmation.token)
        };
        match (&self.pin, confirmation.verify_identity) {
            (Some(pin), true) => {
                let _gate = pin.gate.lock().await;
                pin.vouch(id);
                approve().await;
            }
            _ => approve().await,
        }
    }

    pub async fn reply(&self, id: TaskId, text: &str) {
        let task = self.task(id);
        if let Some(question) = &task.question {
            self.butler
                .reply(id, question.step, &task.intent, text)
                .await;
        }
    }

    pub fn cancel(&self, id: TaskId) -> io::Result<()> {
        self.butler.tasks().cancel(id).map(|_| ())
    }

    /// Everything the audit log holds so far.
    pub fn audit(&self) -> io::Result<Vec<AuditEvent>> {
        Ok(AuditLog::replay(self.dir.join("audit.jsonl"))?
            .into_iter()
            .map(|entry| entry.event)
            .collect())
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        // The scratch log holds a conversation that never happened anywhere
        // real; it must not linger.
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
