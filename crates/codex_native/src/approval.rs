//! Native approval choices. Session grants use Codex's own approval cache;
//! they do not write global rules or grant unrestricted session permissions.
use agent_client_protocol::schema as acp;
use serde_json::Value;

#[derive(Clone, Copy)]
pub(crate) struct ApprovalChoices {
    once: bool,
    session: bool,
}

impl ApprovalChoices {
    pub(crate) fn from_params(params: &Value) -> Self {
        // The installed 0.153.4 command/file request schemas omit this field
        // and their response enums permit both grants. Respect an explicit
        // future/restricted decision list instead of offering unsupported grants.
        let permits = |decision: &str| match params.get("availableDecisions") {
            None | Some(Value::Null) => true,
            Some(Value::Array(decisions)) => decisions
                .iter()
                .any(|value| value.as_str() == Some(decision)),
            Some(_) => false,
        };
        Self {
            once: permits("accept"),
            session: permits("acceptForSession"),
        }
    }

    pub(crate) fn options(self) -> Vec<acp::PermissionOption> {
        let mut options = Vec::new();
        if self.once {
            options.push(acp::PermissionOption::new(
                "allow",
                "Allow once",
                acp::PermissionOptionKind::AllowOnce,
            ));
        }
        if self.session {
            // ACP has no session-specific kind: retain the native scope in
            // the option label/id and translate only to acceptForSession.
            options.push(acp::PermissionOption::new(
                "allow_session",
                "Allow for this session",
                acp::PermissionOptionKind::AllowAlways,
            ));
        }
        options.push(acp::PermissionOption::new(
            "deny",
            "Deny",
            acp::PermissionOptionKind::RejectOnce,
        ));
        options
    }

    pub(crate) fn decision(self, selected: Option<&str>) -> &'static str {
        match selected {
            Some("allow") if self.once => "accept",
            Some("allow_session") if self.session => "acceptForSession",
            _ => "decline",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn command_and_file_defaults_offer_native_session_cache() {
        for params in [
            json!({"command":"echo test"}),
            json!({"grantRoot":"/tmp/example"}),
        ] {
            let choices = ApprovalChoices::from_params(&params);
            let options = choices.options();
            assert_eq!(
                options
                    .iter()
                    .map(|option| option.id.0.as_ref())
                    .collect::<Vec<_>>(),
                ["allow", "allow_session", "deny"]
            );
            assert_eq!(options[1].name, "Allow for this session");
            assert_eq!(choices.decision(Some("allow")), "accept");
            assert_eq!(choices.decision(Some("allow_session")), "acceptForSession");
            assert_eq!(choices.decision(Some("deny")), "decline");
        }
    }

    #[test]
    fn explicit_restrictions_do_not_allow_forged_or_persistent_grants() {
        let choices = ApprovalChoices::from_params(
            &json!({"availableDecisions":["accept","decline", {"acceptWithExecpolicyAmendment":{"execpolicy_amendment":["echo"]}}]}),
        );
        assert_eq!(choices.options().len(), 2);
        assert_eq!(choices.decision(Some("allow")), "accept");
        for selected in [
            Some("allow_session"),
            Some("acceptWithExecpolicyAmendment"),
            Some("unknown"),
            None,
        ] {
            assert_eq!(choices.decision(selected), "decline");
        }
    }

    #[test]
    fn malformed_empty_and_session_only_decision_lists_fail_closed() {
        for available in [json!([]), json!("acceptForSession"), json!(["unknown"])] {
            let choices = ApprovalChoices::from_params(&json!({"availableDecisions":available}));
            assert_eq!(choices.options().len(), 1);
            assert_eq!(choices.decision(Some("allow")), "decline");
            assert_eq!(choices.decision(Some("allow_session")), "decline");
        }
        let choices = ApprovalChoices::from_params(
            &json!({"availableDecisions":["acceptForSession","decline"]}),
        );
        assert_eq!(choices.options().len(), 2);
        assert_eq!(choices.decision(Some("allow")), "decline");
        assert_eq!(choices.decision(Some("allow_session")), "acceptForSession");
    }
}
