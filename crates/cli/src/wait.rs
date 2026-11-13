//! Wait for a task to stop needing the caller's attention.
//!
//! A script or another agent that submits an intent usually wants its outcome,
//! not a task number to poll. Waiting only reads: it never resubmits, approves
//! or cancels, and giving up leaves the task running in the service.

use std::time::Duration;

use agentos_ipc::{Command, Reply};
use agentos_runtime::{Task, TaskId, TaskState};
use tokio::sync::mpsc;
use tokio::time::{timeout, Instant};

use crate::client::{Client, ClientError};

/// Re-read at least this often even when change notifications are arriving,
/// so a missed or delayed notification cannot stall the wait.
const WATCHED_POLL: Duration = Duration::from_secs(2);
/// Without a live subscription the task is simply polled.
const UNWATCHED_POLL: Duration = Duration::from_millis(200);

#[derive(Debug)]
pub enum WaitOutcome {
    /// The task reached a terminal state; inspect `state` for which one.
    Settled(Task),
    /// The task is paused until a human approves a concrete step.
    NeedsApproval(Task),
    /// The task is paused until someone answers the question it asked.
    NeedsInput(Task),
    /// The limit passed first. The task is unaffected and keeps running.
    TimedOut(Task),
}

impl WaitOutcome {
    pub fn task(&self) -> &Task {
        match self {
            Self::Settled(task)
            | Self::NeedsApproval(task)
            | Self::NeedsInput(task)
            | Self::TimedOut(task) => task,
        }
    }

    /// Process exit status for `agentos submit --wait`.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Settled(task) if task.state == TaskState::Succeeded => 0,
            Self::Settled(_) => 3,
            Self::NeedsApproval(_) => 4,
            Self::TimedOut(_) => 5,
            Self::NeedsInput(_) => 6,
        }
    }
}

async fn read_task(client: &Client, id: TaskId) -> Result<Task, ClientError> {
    match client.request(Command::Get { id }).await? {
        Reply::Task { task } => Ok(task),
        _ => unreachable!("the client verifies that a get is answered with a task"),
    }
}

pub async fn wait_for_task(
    client: &Client,
    id: TaskId,
    limit: Duration,
) -> Result<WaitOutcome, ClientError> {
    let deadline = Instant::now() + limit;
    // Subscribe before the first read so no change can fall between the two.
    // The subscription is only a hint to re-read; if it cannot be opened or
    // later ends, the loop below keeps polling the authoritative task.
    let (notify, mut changes) = mpsc::channel::<()>(1);
    let reader = match client.watch(None).await {
        Ok(mut stream) => Some(tokio::spawn(async move {
            // A dedicated task owns the stream: a frame is never half-read by
            // a cancelled wait.
            while let Ok(Some(_)) = stream.next().await {
                if notify.try_send(()).is_err() && notify.is_closed() {
                    break;
                }
            }
        })),
        Err(_) => {
            drop(notify);
            None
        }
    };
    let mut watching = reader.is_some();
    let outcome = loop {
        let task = match read_task(client, id).await {
            Ok(task) => task,
            Err(error) => break Err(error),
        };
        if task.state.is_terminal() {
            break Ok(WaitOutcome::Settled(task));
        }
        if task.state == TaskState::AwaitingConfirmation {
            break Ok(WaitOutcome::NeedsApproval(task));
        }
        if task.state == TaskState::AwaitingInput {
            break Ok(WaitOutcome::NeedsInput(task));
        }
        let now = Instant::now();
        if now >= deadline {
            break Ok(WaitOutcome::TimedOut(task));
        }
        let pause = if watching {
            WATCHED_POLL
        } else {
            UNWATCHED_POLL
        }
        .min(deadline - now);
        if watching {
            if let Ok(None) = timeout(pause, changes.recv()).await {
                watching = false;
            }
        } else {
            tokio::time::sleep(pause).await;
        }
    };
    if let Some(reader) = reader {
        reader.abort();
    }
    outcome
}
