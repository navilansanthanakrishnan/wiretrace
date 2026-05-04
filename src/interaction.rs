use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::Result;
use rdev::{Button, Event, EventType, Key, listen};
use serde::Serialize;

use crate::cli::InteractionMode;

const DEFAULT_IDLE_TIMEOUT_MS: u64 = 1_200;
const DEFAULT_MAX_DURATION_MS: u64 = 10_000;
const DEFAULT_TRIGGER_DEBOUNCE_MS: u64 = 250;

#[derive(Debug, Clone, Serialize)]
pub struct InteractionContext {
    pub id: u64,
    pub started_at_ms: u128,
    pub trigger: String,
    pub app_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct InteractionCapture {
    config: InteractionConfig,
    state: Arc<Mutex<InteractionState>>,
}

#[derive(Debug, Clone)]
struct InteractionConfig {
    mode: InteractionMode,
    first_request_window_ms: u64,
    idle_timeout_ms: u64,
    max_duration_ms: u64,
    trigger_debounce_ms: u64,
}

#[derive(Debug, Default)]
struct InteractionState {
    next_id: u64,
    active: Option<ActiveInteraction>,
    last_trigger_kind: Option<String>,
    last_trigger_ms: u128,
}

#[derive(Debug)]
struct ActiveInteraction {
    context: InteractionContext,
    first_request_deadline_ms: u128,
    hard_deadline_ms: u128,
    last_capture_ms: Option<u128>,
    matched_request_count: u64,
}

impl InteractionCapture {
    pub fn new(mode: InteractionMode, first_request_window_ms: u64) -> Self {
        Self {
            config: InteractionConfig {
                mode,
                first_request_window_ms,
                idle_timeout_ms: DEFAULT_IDLE_TIMEOUT_MS,
                max_duration_ms: DEFAULT_MAX_DURATION_MS,
                trigger_debounce_ms: DEFAULT_TRIGGER_DEBOUNCE_MS,
            },
            state: Arc::new(Mutex::new(InteractionState::default())),
        }
    }

    pub fn mode(&self) -> InteractionMode {
        self.config.mode
    }

    pub fn first_request_window_ms(&self) -> u64 {
        self.config.first_request_window_ms
    }

    pub fn idle_timeout_ms(&self) -> u64 {
        self.config.idle_timeout_ms
    }

    pub fn max_duration_ms(&self) -> u64 {
        self.config.max_duration_ms
    }

    pub fn arm_manual(&self) -> InteractionContext {
        self.register_trigger("manual", None)
    }

    pub fn observe_request_start(&self) -> Option<InteractionContext> {
        match self.config.mode {
            InteractionMode::Off => None,
            InteractionMode::Manual | InteractionMode::Auto => self.observe_active_interaction(),
        }
    }

    pub fn start_auto_listener(&self) -> Result<()> {
        if self.config.mode != InteractionMode::Auto {
            return Ok(());
        }

        #[cfg(not(target_os = "macos"))]
        {
            return Err(anyhow::anyhow!(
                "auto interaction mode currently requires macOS global input hooks"
            ));
        }

        #[cfg(target_os = "macos")]
        {
            let interaction = self.clone();
            thread::Builder::new()
                .name("agent-mcp-b-input-listener".into())
                .spawn(move || {
                    let callback = move |event: Event| {
                        if let Some(trigger_kind) = classify_trigger(&event.event_type) {
                            interaction.handle_auto_trigger(trigger_kind);
                        }
                    };

                    if let Err(error) = listen(callback) {
                        eprintln!(
                            "auto interaction listener failed: {error:?}. ensure Terminal has Accessibility permission in System Settings > Privacy & Security > Accessibility"
                        );
                    }
                })
                .map(|_| ())
                .map_err(Into::into)
        }
    }

    fn handle_auto_trigger(&self, trigger_kind: &'static str) {
        let now = now_ms();

        let mut state = self
            .state
            .lock()
            .expect("interaction state mutex should not be poisoned");
        expire_if_needed(&mut state.active, &self.config, now);

        if let Some(active) = state.active.as_ref() {
            if active.matched_request_count > 0
                && interaction_is_still_active(active, &self.config, now)
            {
                return;
            }
        }

        if state.last_trigger_kind.as_deref() == Some(trigger_kind)
            && now.saturating_sub(state.last_trigger_ms) <= self.config.trigger_debounce_ms as u128
        {
            return;
        }

        state.last_trigger_kind = Some(trigger_kind.to_string());
        state.last_trigger_ms = now;
        drop(state);

        let app_name = frontmost_app_name();
        let context = self.register_trigger(trigger_kind, app_name);
        println!(
            "detected interaction #{} via {}{}",
            context.id,
            context.trigger,
            context
                .app_name
                .as_ref()
                .map(|name| format!(" in {name}"))
                .unwrap_or_default()
        );
    }

    fn observe_active_interaction(&self) -> Option<InteractionContext> {
        let now = now_ms();
        let mut state = self
            .state
            .lock()
            .expect("interaction state mutex should not be poisoned");
        expire_if_needed(&mut state.active, &self.config, now);

        let active = state.active.as_mut()?;

        if active.matched_request_count == 0 {
            if now > active.first_request_deadline_ms {
                state.active = None;
                return None;
            }
        } else if !interaction_is_still_active(active, &self.config, now) {
            state.active = None;
            return None;
        }

        active.matched_request_count += 1;
        active.last_capture_ms = Some(now);
        Some(active.context.clone())
    }

    fn register_trigger(
        &self,
        trigger: impl Into<String>,
        app_name: Option<String>,
    ) -> InteractionContext {
        let now = now_ms();
        let trigger = trigger.into();
        let mut state = self
            .state
            .lock()
            .expect("interaction state mutex should not be poisoned");

        state.next_id += 1;
        let context = InteractionContext {
            id: state.next_id,
            started_at_ms: now,
            trigger,
            app_name,
        };

        state.active = Some(ActiveInteraction {
            context: context.clone(),
            first_request_deadline_ms: now + self.config.first_request_window_ms as u128,
            hard_deadline_ms: now + self.config.max_duration_ms as u128,
            last_capture_ms: None,
            matched_request_count: 0,
        });

        context
    }
}

fn interaction_is_still_active(
    active: &ActiveInteraction,
    config: &InteractionConfig,
    now: u128,
) -> bool {
    let Some(last_capture_ms) = active.last_capture_ms else {
        return false;
    };

    now <= active.hard_deadline_ms
        && now.saturating_sub(last_capture_ms) <= config.idle_timeout_ms as u128
}

fn expire_if_needed(
    active: &mut Option<ActiveInteraction>,
    config: &InteractionConfig,
    now: u128,
) {
    let Some(current) = active.as_ref() else {
        return;
    };

    let expired = if current.matched_request_count == 0 {
        now > current.first_request_deadline_ms
    } else {
        !interaction_is_still_active(current, config, now)
    };

    if expired {
        *active = None;
    }
}

fn classify_trigger(event_type: &EventType) -> Option<&'static str> {
    match event_type {
        EventType::ButtonPress(Button::Left) => Some("mouse_left_down"),
        EventType::ButtonPress(Button::Right) => Some("mouse_right_down"),
        EventType::ButtonPress(Button::Middle) => Some("mouse_middle_down"),
        EventType::KeyPress(key) => classify_key_trigger(key),
        _ => None,
    }
}

fn classify_key_trigger(key: &Key) -> Option<&'static str> {
    match key {
        Key::Return | Key::Space | Key::Tab => Some("key_submit"),
        Key::LeftArrow | Key::RightArrow | Key::UpArrow | Key::DownArrow => Some("key_nav"),
        Key::Escape => Some("key_escape"),
        Key::Backspace | Key::Delete => Some("key_edit"),
        _ => Some("key_press"),
    }
}

fn frontmost_app_name() -> Option<String> {
    #[cfg(not(target_os = "macos"))]
    {
        None
    }

    #[cfg(target_os = "macos")]
    {
        let output = Command::new("osascript")
            .args([
                "-e",
                r#"tell application "System Events" to get name of first application process whose frontmost is true"#,
            ])
            .output()
            .ok()?;

        if !output.status.success() {
            return None;
        }

        let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if value.is_empty() {
            None
        } else {
            Some(value)
        }
    }
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::InteractionCapture;
    use crate::cli::InteractionMode;

    #[test]
    fn manual_interaction_requires_arming() {
        let interaction = InteractionCapture::new(InteractionMode::Manual, 1000);
        assert!(interaction.observe_request_start().is_none());

        let context = interaction.arm_manual();
        let captured = interaction.observe_request_start().expect("manual interaction should capture once armed");
        assert_eq!(captured.id, context.id);
        assert_eq!(captured.trigger, "manual");
    }

    #[test]
    fn interaction_session_stays_open_across_request_cascade() {
        let interaction = InteractionCapture::new(InteractionMode::Manual, 1000);
        let context = interaction.arm_manual();

        let first = interaction.observe_request_start().expect("first request should capture");
        let second = interaction.observe_request_start().expect("second request in burst should capture");

        assert_eq!(first.id, context.id);
        assert_eq!(second.id, context.id);
    }
}
