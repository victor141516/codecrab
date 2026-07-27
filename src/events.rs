use tokio::sync::oneshot;

pub(crate) enum AgentEvent {
    ApprovalRequested {
        action: String,
        response: oneshot::Sender<bool>,
    },
}
