//! Single executor thread: resolves a matched candidate and runs its plan.
use super::actions::execute_inner;
use super::*;
pub(super) struct Candidate {
    pub(super) rule: Rule,
    pub(super) matched: MatchResult,
    pub(super) proof: CandidateProof,
    pub(super) focus: Foreground,
    pub(super) generation: u64,
    pub(super) terminator: Option<String>,
}
#[derive(Clone)]
pub(super) enum CandidateProof {
    Espanso(rawinput::Proof),
}
static SENDER: OnceLock<mpsc::SyncSender<Candidate>> = OnceLock::new();
pub(super) fn enqueue(shared: &Shared, candidate: Candidate) -> bool {
    if shared
        .busy
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return false;
    }
    shared.cancel.store(false, Ordering::SeqCst);
    if SENDER
        .get()
        .is_none_or(|sender| sender.try_send(candidate).is_err())
    {
        shared.busy.store(false, Ordering::SeqCst);
        return false;
    }
    true
}
pub(crate) fn start_executor(shared: Arc<Shared>) {
    let (tx, rx) = mpsc::sync_channel::<Candidate>(1);
    SENDER.set(tx).ok();
    std::thread::spawn(move || {
        for c in rx {
            let mut ctx = VariableContext {
                clipboard: arboard::Clipboard::new()
                    .ok()
                    .and_then(|mut cb| cb.get_text().ok()),
                ..Default::default()
            };
            let selection_needed = serde_json::to_string(&c.rule.action)
                .unwrap_or_default()
                .contains("{selection}")
                || matches!(c.rule.action, Action::Reusable { .. })
                    && serde_json::to_string(&*shared.config.lock().unwrap())
                        .unwrap_or_default()
                        .contains("{selection}");
            if selection_needed {
                ctx.selection = capture_selection().ok();
            }
            ctx.parameters.insert(
                "action_parameter".into(),
                c.matched
                    .parameter
                    .clone()
                    .map(serde_json::Value::String)
                    .unwrap_or(serde_json::Value::Null),
            );
            let config = shared.config.lock().unwrap().clone();
            let plan = expand_reusable(&c.rule.action, &config, &ctx);
            let result = match plan {
                Ok(plan) => execute_inner(
                    shared.clone(),
                    plan,
                    c.focus.clone(),
                    c.generation,
                    Some(c.proof.clone()),
                    if c.matched.consume_terminator {
                        None
                    } else {
                        c.terminator.clone()
                    },
                    match c.rule.action {
                        Action::Replay { speed, .. } => speed,
                        _ => 1.0,
                    },
                ),
                Err(errors) => Err(errors
                    .iter()
                    .map(|e| e.message.clone())
                    .collect::<Vec<_>>()
                    .join("; ")),
            };
            if let Ok(completed) = &result {
                let mut result = completed.clone();
                result.id = format!("{}:{}", c.rule.id, c.generation);
                *shared.last.lock().unwrap() = Some(result);
            }
            if let Err(message) = result {
                *shared.last.lock().unwrap() = Some(ExecutionResult {
                    id: format!("{}:{}", c.rule.id, c.generation),
                    status: "failed".into(),
                    message,
                    completed_steps: 0,
                    side_effects: false,
                })
            }
            shared.busy.store(false, Ordering::SeqCst);
        }
    });
}
