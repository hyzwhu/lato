use serde_json::Value;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HookEvent {
    SessionStart,
    SessionEnd,
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    Stop,
    PreCompact,
    PostCompact,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HookDecision {
    Continue,
    Deny(String),
    Ask,
    Rewrite(Value),
}

pub trait Hook: Send + Sync {
    fn call(&self, event: HookEvent, tool_name: Option<&str>, payload: &Value) -> HookDecision;
}

#[derive(Default)]
pub struct HookBus {
    hooks: Vec<Box<dyn Hook>>,
}

impl HookBus {
    pub fn register(&mut self, hook: Box<dyn Hook>) {
        self.hooks.push(hook);
    }

    pub fn emit(&self, event: HookEvent, tool_name: Option<&str>, payload: &Value) -> HookDecision {
        let mut rewritten = payload.clone();
        for hook in &self.hooks {
            match hook.call(event, tool_name, &rewritten) {
                HookDecision::Continue => {}
                HookDecision::Rewrite(next) => rewritten = next,
                decision @ (HookDecision::Deny(_) | HookDecision::Ask) => return decision,
            }
        }
        if &rewritten != payload {
            HookDecision::Rewrite(rewritten)
        } else {
            HookDecision::Continue
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct RenameArgument;
    impl Hook for RenameArgument {
        fn call(
            &self,
            event: HookEvent,
            _tool_name: Option<&str>,
            _payload: &Value,
        ) -> HookDecision {
            if event == HookEvent::PreToolUse {
                HookDecision::Rewrite(serde_json::json!({"path":"safe.txt"}))
            } else {
                HookDecision::Continue
            }
        }
    }

    #[test]
    fn e5_1_pre_tool_hook_can_rewrite_arguments_not_tool_name() {
        let mut bus = HookBus::default();
        bus.register(Box::new(RenameArgument));
        let decision = bus.emit(
            HookEvent::PreToolUse,
            Some("search_replace"),
            &serde_json::json!({"path":"unsafe.txt"}),
        );
        assert_eq!(
            decision,
            HookDecision::Rewrite(serde_json::json!({"path":"safe.txt"}))
        );
    }
}
