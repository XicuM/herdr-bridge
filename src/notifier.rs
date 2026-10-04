use crate::herdr::HerdrClient;
use reqwest::Client;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex};
use tokio::time::{interval, Duration};
use tracing::{error, info, warn};

pub struct Notifier {
    ntfy_topic: Option<String>,
    pushover_user: Option<String>,
    pushover_token: Option<String>,
    http_client: Client,
    last_statuses: Mutex<HashMap<String, String>>,
}

impl Notifier {
    pub fn new(
        ntfy_topic: Option<String>,
        pushover_user: Option<String>,
        pushover_token: Option<String>,
    ) -> Self {
        Self {
            ntfy_topic,
            pushover_user,
            pushover_token,
            http_client: Client::new(),
            last_statuses: Mutex::new(HashMap::new()),
        }
    }

    pub fn start(self: Arc<Self>, herdr: Arc<HerdrClient>, mut event_rx: broadcast::Receiver<Value>) {
        if self.ntfy_topic.is_none() && self.pushover_user.is_none() {
            info!("No push notification topic configured (ntfy or pushover). Background alerts disabled.");
            return;
        }

        // Listener 1: WebSocket / socket broadcast event listener
        let notifier_events = self.clone();
        tokio::spawn(async move {
            info!("Notification event watcher started");
            loop {
                match event_rx.recv().await {
                    Ok(event) => {
                        notifier_events.handle_event(&event).await;
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        warn!("Notification watcher lagged behind by {} events", skipped);
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        break;
                    }
                }
            }
        });

        // Listener 2: Periodic snapshot watcher for robust state transition detection
        let notifier_poll = self.clone();
        tokio::spawn(async move {
            info!("Notification status poller started");
            let mut tick = interval(Duration::from_millis(1500));
            loop {
                tick.tick().await;
                if let Ok(snap) = herdr.snapshot().await {
                    notifier_poll.check_snapshot(&snap).await;
                }
            }
        });
    }

    async fn check_snapshot(&self, snap: &Value) {
        let snapshot = snap.get("snapshot").unwrap_or(snap);
        if let Some(panes) = snapshot.get("panes").and_then(|v| v.as_array()) {
            let mut last_statuses = self.last_statuses.lock().await;

            for pane in panes {
                let pane_id = pane.get("pane_id").and_then(|v| v.as_str()).unwrap_or("");
                let status = pane.get("agent_status").and_then(|v| v.as_str()).unwrap_or("unknown");
                let title = pane.get("terminal_title").and_then(|v| v.as_str()).unwrap_or(pane_id);

                if pane_id.is_empty() {
                    continue;
                }

                let prev_status = last_statuses.insert(pane_id.to_string(), status.to_string());

                if let Some(prev) = prev_status {
                    if prev != status {
                        if status == "blocked" {
                            info!("Status change for {}: {} -> {}", pane_id, prev, status);
                            self.send_alert(
                                &format!("⚠️ Agent Blocked: {}", title),
                                &format!("Agent in pane {} is waiting for your approval or input!", pane_id),
                                "urgent",
                                "warning,robot",
                            ).await;
                        } else if status == "done" && prev == "working" {
                            info!("Status change for {}: {} -> {}", pane_id, prev, status);
                            self.send_alert(
                                &format!("✅ Agent Completed: {}", title),
                                &format!("Agent in pane {} has finished its task.", pane_id),
                                "default",
                                "white_check_mark,robot",
                            ).await;
                        }
                    }
                }
            }
        }
    }

    async fn handle_event(&self, event: &Value) {
        let status = event.pointer("/event/agent_status")
            .or_else(|| event.get("agent_status"))
            .and_then(|v| v.as_str());

        let pane_id = event.pointer("/event/pane_id")
            .or_else(|| event.get("pane_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");

        let agent_name = event.pointer("/event/agent")
            .or_else(|| event.get("agent"))
            .and_then(|v| v.as_str())
            .unwrap_or(pane_id);

        if let Some(status) = status {
            match status {
                "blocked" => {
                    self.send_alert(
                        &format!("⚠️ Agent Blocked: {}", agent_name),
                        &format!("Agent in pane {} is waiting for your approval or input!", pane_id),
                        "urgent",
                        "warning,robot",
                    ).await;
                }
                "done" => {
                    self.send_alert(
                        &format!("✅ Agent Completed: {}", agent_name),
                        &format!("Agent in pane {} has finished its task.", pane_id),
                        "default",
                        "white_check_mark,robot",
                    ).await;
                }
                _ => {}
            }
        }
    }

    async fn send_alert(&self, title: &str, message: &str, priority: &str, tags: &str) {
        if let Some(topic) = &self.ntfy_topic {
            let url = format!("https://ntfy.sh/{}", topic);
            match self.http_client
                .post(&url)
                .header("Title", title)
                .header("Priority", priority)
                .header("Tags", tags)
                .body(message.to_string())
                .send()
                .await
            {
                Ok(resp) => {
                    if resp.status().is_success() {
                        info!("ntfy.sh push sent successfully to topic: {}", topic);
                    } else {
                        error!("ntfy.sh returned error code: {}", resp.status());
                    }
                }
                Err(e) => {
                    error!("Failed to send ntfy push: {}", e);
                }
            }
        }

        if let (Some(user), Some(token)) = (&self.pushover_user, &self.pushover_token) {
            let url = "https://api.pushover.net/1/messages.json";
            let form = [
                ("token", token.as_str()),
                ("user", user.as_str()),
                ("title", title),
                ("message", message),
            ];
            if let Err(e) = self.http_client.post(url).form(&form).send().await {
                error!("Failed to send Pushover alert: {}", e);
            }
        }
    }
}
