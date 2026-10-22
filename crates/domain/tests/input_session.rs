use quicker_domain::*;

fn observed(text: &str, at: u64) -> TextObservation {
    TextObservation {
        revision: 7,
        focus_id: "window:1".into(),
        process: "synthetic.exe".into(),
        control_id: "editor:3".into(),
        caret_anchor: "navigation:1".into(),
        selection_collapsed: true,
        committed_suffix: text.into(),
        composition: CompositionEvidence::Inactive,
        observed_at_ms: at,
        self_injected: false,
    }
}
fn session(text: &str) -> InputSession {
    let mut session = InputSession::new(1024, 250);
    session.observe(observed("existing ", 1), 1);
    session.observe(observed(&format!("existing {text}"), 2), 2);
    session
}
fn stamp(session: &InputSession) -> EnvironmentStamp {
    let observed = session.last_observation().unwrap();
    EnvironmentStamp {
        revision: observed.revision,
        buffer_generation: session.generation(),
        focus_id: observed.focus_id.clone(),
        process: observed.process.clone(),
        // Even unknown legacy flags cannot replace the real observation proof.
        ime_state: ImeState::Unknown,
        injection_safe: true,
    }
}
#[test]
fn initial_observation_does_not_trigger_historic_text() {
    let mut s = InputSession::new(1024, 250);
    assert_eq!(
        s.observe(observed("old abbreviation", 1), 1),
        ObservationDelta::Reset
    );
    assert_eq!(s.matching_input(), None);
    assert!(s
        .erasure_proof("abbreviation", ImeErasure::Backspace, 1)
        .is_err());
}
#[test]
fn observed_ascii_and_chinese_commit_match_without_vk_inference() {
    for text in ["ascii", "中文", "中文🙂", "e\u{301}", "👩‍💻"] {
        let s = session(text);
        assert_eq!(s.matching_input(), Some(text));
        // Escape preference controls candidate cancellation, not deletion of committed body.
        let proof = s.erasure_proof(text, ImeErasure::Escape, 2).unwrap();
        assert_eq!(
            proof.kind(),
            ErasureKind::CommittedText {
                scalar_count: text.chars().count()
            }
        );
        assert_eq!(proof.expected_suffix(), "existing ");
        let after = observed("existing ", 3);
        assert!(proof.verify_after(&after, 3).is_ok());
    }
}
#[test]
fn native_observation_flows_into_compiled_matching() {
    let s = session("中文");
    let c = Config {
        revision: 7,
        rules: vec![Rule {
            id: "chinese".into(),
            match_spec: MatchSpec {
                kind: MatchKind::Literal,
                value: "中文".into(),
            },
            ..Rule::default()
        }],
        ..Config::default()
    };
    let compiled = compile_config(&c).unwrap();
    let matched = compiled
        .match_input(
            s.matching_input().unwrap(),
            Some("space"),
            &ForegroundContext {
                process: "synthetic.exe".into(),
                ..ForegroundContext::default()
            },
        )
        .unwrap();
    assert!(s
        .erasure_proof(&matched.matched, ImeErasure::Escape, 2)
        .is_ok());
}
#[test]
fn append_differences_preserve_unicode_and_invalidate_old_proof() {
    let mut s = session("中🙂");
    let proof = s.erasure_proof("中🙂", ImeErasure::Backspace, 2).unwrap();
    assert_eq!(
        s.observe(observed("existing 中🙂文", 3), 3),
        ObservationDelta::CommittedAppend("文".into())
    );
    assert_eq!(s.matching_input(), Some("中🙂文"));
    assert_eq!(
        proof.verify_before(s.last_observation().unwrap(), s.generation(), 3),
        Err("stale_buffer")
    );
}
#[test]
fn editing_selection_caret_focus_control_and_revision_reset_the_session() {
    for change in 0..8 {
        let mut s = session("abc");
        let mut current = observed("existing abc", 3);
        match change {
            0 => current.committed_suffix = "existing axc".into(),
            1 => current.selection_collapsed = false,
            2 => current.caret_anchor = "navigation:2".into(),
            3 => current.focus_id = "window:2".into(),
            4 => current.control_id = "editor:4".into(),
            5 => current.process = "other.exe".into(),
            6 => current.revision += 1,
            7 => current.self_injected = true,
            _ => unreachable!(),
        }
        assert_eq!(s.observe(current, 3), ObservationDelta::Reset);
        assert_eq!(s.matching_input(), None);
    }
}
#[test]
fn provider_timeout_unknown_and_reordered_snapshots_fail_closed() {
    let mut s = session("abc");
    let proof = s.erasure_proof("abc", ImeErasure::Escape, 2).unwrap();
    assert_eq!(
        proof.verify_before(&observed("existing abc", 2), s.generation(), 253),
        Err("observation_unavailable")
    );
    assert!(proof
        .verify_before(&observed("existing abc", 253), s.generation(), 253)
        .is_ok());
    assert!(s.erasure_proof("abc", ImeErasure::Escape, 252).is_err());
    assert_eq!(
        s.observe(observed("existing abcd", 1), 3),
        ObservationDelta::Reset
    );
    assert_eq!(s.matching_input(), None);
    let mut unknown = observed("existing abc", 4);
    unknown.composition = CompositionEvidence::Unknown;
    s.observe(unknown, 4);
    assert_eq!(s.last_observation(), None);
    assert_eq!(
        s.observe(observed("existing abc", 1), 300),
        ObservationDelta::Reset
    );
}
fn candidate(body: &str, text: &str, at: u64, safe: bool) -> TextObservation {
    let mut current = observed(body, at);
    current.composition = CompositionEvidence::Active {
        text: text.into(),
        body_unchanged: safe,
    };
    current
}
#[test]
fn confirmed_pure_candidate_escape_requires_unchanged_body_readback() {
    let mut s = InputSession::new(1024, 250);
    s.observe(observed("body🙂", 1), 1);
    assert_eq!(
        s.observe(candidate("body🙂", "nihao", 2, true), 2),
        ObservationDelta::CompositionUpdated("nihao".into())
    );
    let proof = s.erasure_proof("nihao", ImeErasure::Escape, 2).unwrap();
    assert_eq!(proof.kind(), ErasureKind::CancelComposition);
    assert!(proof
        .verify_before(s.last_observation().unwrap(), s.generation(), 2)
        .is_ok());
    assert!(proof.verify_after(&observed("body🙂", 3), 3).is_ok());
    assert_eq!(
        proof.verify_after(&observed("body", 3), 3),
        Err("erasure_not_observed")
    );
    assert!(proof
        .verify_after(&candidate("body🙂", "nihao", 3, true), 3)
        .is_err());
    assert!(s.erasure_proof("nihao", ImeErasure::Backspace, 2).is_err());
    assert!(s.erasure_proof("hao", ImeErasure::Escape, 2).is_err());
}
#[test]
fn partially_committed_unknown_or_unanchored_candidate_never_deletes() {
    for (body, confirmed) in [("body中", true), ("body", false)] {
        let mut s = InputSession::new(1024, 250);
        s.observe(observed("body", 1), 1);
        assert_eq!(
            s.observe(candidate(body, "wen", 2, confirmed), 2),
            ObservationDelta::Reset
        );
        assert!(s.erasure_proof("wen", ImeErasure::Escape, 2).is_err());
    }
    let mut s = InputSession::new(1024, 250);
    s.observe(candidate("body", "wen", 1, true), 1);
    s.observe(candidate("body", "wen2", 2, true), 2);
    assert_eq!(s.matching_input(), None);
}
#[test]
fn phonetic_candidate_is_replaced_by_actual_chinese_commit() {
    let mut s = InputSession::new(1024, 250);
    s.observe(observed("body ", 1), 1);
    s.observe(candidate("body ", "zhongwen", 2, true), 2);
    assert_eq!(
        s.observe(observed("body 中文", 3), 3),
        ObservationDelta::CommittedAppend("中文".into())
    );
    assert_eq!(s.matching_input(), Some("中文"));
    assert!(s.erasure_proof("中文", ImeErasure::Escape, 3).is_ok());
    assert!(s.erasure_proof("zhongwen", ImeErasure::Escape, 3).is_err());
}
#[test]
fn canceled_candidate_and_shifted_bounded_suffix_reset_matching() {
    let mut s = session("abc");
    s.observe(candidate("existing abc", "nihao", 3, true), 3);
    s.observe(observed("existing abc", 4), 4);
    assert_eq!(s.matching_input(), None);
    let mut s = session("abc");
    s.observe(observed("isting abcd", 3), 3);
    assert_eq!(s.matching_input(), None);
}
#[test]
fn buffer_capacity_resets_at_character_boundary() {
    let mut s = InputSession::new(2, 250);
    s.observe(observed("", 1), 1);
    s.observe(observed("🙂中", 2), 2);
    assert_eq!(s.matching_input(), Some("🙂中"));
    assert_eq!(s.observe(observed("🙂中文", 3), 3), ObservationDelta::Reset);
    assert_eq!(s.matching_input(), None);
}
#[test]
fn proof_revalidates_every_target_field_and_fresh_after_observation() {
    let s = session("中文🙂");
    let proof = s.erasure_proof("中文🙂", ImeErasure::Backspace, 2).unwrap();
    assert_eq!(
        proof.verify_after(&observed("existing ", 2), 2),
        Err("observation_not_fresh")
    );
    for change in 0..7 {
        let mut current = observed("existing 中文🙂", 3);
        match change {
            0 => current.committed_suffix = "existing 中文🙃".into(),
            1 => current.revision += 1,
            2 => current.caret_anchor = "navigation:2".into(),
            3 => current.control_id = "editor:4".into(),
            4 => current.selection_collapsed = false,
            5 => current.self_injected = true,
            6 => current.composition = CompositionEvidence::Unknown,
            _ => unreachable!(),
        }
        assert!(proof.verify_before(&current, s.generation(), 3).is_err());
    }
}
#[test]
fn transaction_uses_proof_and_only_marks_verified_deletion_once() {
    let s = session("中文🙂");
    let proof = s.erasure_proof("中文🙂", ImeErasure::Escape, 2).unwrap();
    let current = stamp(&s);
    let mut tx = Transaction::new("rule", current.clone(), 100);
    tx.mark_ready().unwrap();
    assert!(tx
        .check_before_erase_with_proof(&current, &proof, s.last_observation().unwrap(), 2)
        .is_ok());
    assert!(tx
        .mark_erased_with_proof(&current, &proof, &observed("existing 中", 3), 3)
        .is_err());
    assert!(!tx.report.trigger_erased);
    tx.mark_erased_with_proof(&current, &proof, &observed("existing ", 4), 4)
        .unwrap();
    assert!(tx.report.trigger_erased);
    assert_eq!(
        tx.mark_erased_with_proof(&current, &proof, &observed("existing ", 5), 5),
        Err("invalid_phase")
    );
}
#[test]
fn transaction_stale_revision_generation_focus_cancel_timeout_and_injection_reject() {
    let s = session("abc");
    let proof = s.erasure_proof("abc", ImeErasure::Backspace, 2).unwrap();
    for change in 0..7 {
        let mut current = stamp(&s);
        let mut tx = Transaction::new("rule", current.clone(), 100);
        tx.mark_ready().unwrap();
        let mut now = 2;
        match change {
            0 => current.revision += 1,
            1 => current.buffer_generation += 1,
            2 => current.focus_id = "window:2".into(),
            3 => current.process = "other.exe".into(),
            4 => tx.cancel(),
            5 => now = 100,
            6 => current.injection_safe = false,
            _ => unreachable!(),
        }
        assert!(tx
            .check_before_erase_with_proof(&current, &proof, s.last_observation().unwrap(), now)
            .is_err());
    }
}
#[test]
fn missing_and_null_rule_ime_settings_inherit_but_legacy_strings_override() {
    let global = GlobalSettings {
        ime_erasure: ImeErasure::Backspace,
        ..GlobalSettings::default()
    };
    for json in ["{}", r#"{"ime_erasure":null}"#] {
        let rule: Rule = serde_json::from_str(json).unwrap();
        assert_eq!(rule.ime_erasure, None);
        assert_eq!(rule.effective_ime_erasure(&global), ImeErasure::Backspace);
    }
    let rule: Rule = serde_json::from_str(r#"{"ime_erasure":"escape"}"#).unwrap();
    assert_eq!(rule.ime_erasure, Some(ImeErasure::Escape));
    assert_eq!(rule.effective_ime_erasure(&global), ImeErasure::Escape);
    assert_eq!(
        serde_json::to_value(&rule).unwrap()["ime_erasure"],
        "escape"
    );
}
