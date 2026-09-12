use crate::client::SessionSummary;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResumeResolution {
    Match(String),
    Ambiguous(Vec<SessionSummary>),
    Missing,
}

pub fn resolve_resume_reference(reference: &str, sessions: &[SessionSummary]) -> ResumeResolution {
    if let Some(session) = sessions
        .iter()
        .find(|session| session.session_id == reference)
    {
        return ResumeResolution::Match(session.session_id.clone());
    }

    let mut matches = sessions
        .iter()
        .filter(|session| session.title == reference)
        .cloned()
        .collect::<Vec<_>>();
    matches.sort_by(|left, right| {
        right
            .updated_at_ms
            .cmp(&left.updated_at_ms)
            .then_with(|| right.session_id.cmp(&left.session_id))
    });
    match matches.len() {
        0 => ResumeResolution::Missing,
        1 => ResumeResolution::Match(matches[0].session_id.clone()),
        _ => ResumeResolution::Ambiguous(matches),
    }
}

pub fn resume_choice_label(session: &SessionSummary) -> String {
    format!(
        "{} · {} · {}",
        session.title,
        format_updated_at(session.updated_at_ms),
        session.session_id
    )
}

fn format_updated_at(timestamp_ms: u64) -> String {
    let total_seconds = timestamp_ms / 1_000;
    let days = total_seconds / 86_400;
    let rem = total_seconds % 86_400;
    let hour = rem / 3_600;
    let minute = (rem % 3_600) / 60;
    let second = rem % 60;
    let (year, month, day) = civil_from_days(days as i64);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} UTC")
}

fn civil_from_days(days: i64) -> (i64, u64, u64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::{ResumeResolution, resolve_resume_reference, resume_choice_label};
    use crate::client::SessionSummary;

    fn session(id: &str, title: &str, updated_at_ms: u64) -> SessionSummary {
        SessionSummary {
            session_id: id.into(),
            title: title.into(),
            title_source: "manual".into(),
            created_at_ms: 1,
            updated_at_ms,
        }
    }

    #[test]
    fn exact_session_id_resolves_directly() {
        let sessions = [session("s-old", "Older", 10), session("s-new", "Newer", 20)];
        assert_eq!(
            resolve_resume_reference("s-old", &sessions),
            ResumeResolution::Match("s-old".into())
        );
    }

    #[test]
    fn unique_exact_title_resolves_directly() {
        let sessions = [
            session("s-1", "Fix parser", 10),
            session("s-2", "Other work", 20),
        ];
        assert_eq!(
            resolve_resume_reference("Fix parser", &sessions),
            ResumeResolution::Match("s-1".into())
        );
    }

    #[test]
    fn session_id_wins_over_a_title_collision() {
        let sessions = [
            session("target-id", "Other", 1),
            session("other-id", "target-id", 99),
        ];
        assert_eq!(
            resolve_resume_reference("target-id", &sessions),
            ResumeResolution::Match("target-id".into())
        );
    }

    #[test]
    fn duplicate_titles_are_ambiguous_and_ordered_by_recency() {
        let sessions = [
            session("s-older", "Shared title", 10),
            session("s-newer", "Shared title", 30),
            session("s-middle", "Shared title", 20),
            session("s-other", "Different", 40),
        ];
        assert_eq!(
            resolve_resume_reference("Shared title", &sessions),
            ResumeResolution::Ambiguous(vec![
                session("s-newer", "Shared title", 30),
                session("s-middle", "Shared title", 20),
                session("s-older", "Shared title", 10),
            ])
        );
    }

    #[test]
    fn title_matching_is_exact_and_case_sensitive() {
        let sessions = [session("s-1", "Fix Parser", 10)];
        assert_eq!(
            resolve_resume_reference("fix parser", &sessions),
            ResumeResolution::Missing
        );
        assert_eq!(
            resolve_resume_reference("Fix", &sessions),
            ResumeResolution::Missing
        );
    }

    #[test]
    fn unknown_references_are_missing() {
        let sessions = [session("s-1", "Known", 10)];
        assert_eq!(
            resolve_resume_reference("missing", &sessions),
            ResumeResolution::Missing
        );
    }

    #[test]
    fn choice_labels_include_title_update_time_and_complete_id() {
        let session = session("s1788336000000-1", "Shared title", 1_700_000_000_000);
        let label = resume_choice_label(&session);
        assert!(label.contains("Shared title"), "{label}");
        assert!(label.contains("s1788336000000-1"), "{label}");
        assert!(label.contains(" · "), "{label}");
        assert_ne!(label, "Shared title");
    }
}
