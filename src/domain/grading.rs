use thiserror::Error;

use super::{ThemaContent, ThemaKind};

pub(crate) const MATCHER_VERSION: u32 = 1;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum StructuredResponse {
    Text(String),
    Texts(Vec<String>),
    Choice(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Grade {
    pub(crate) matcher_version: u32,
    pub(crate) success: bool,
    pub(crate) revealed_answer: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ClozeCheck {
    pub(crate) matches: Vec<bool>,
    pub(crate) all_entered_correct: bool,
    pub(crate) complete: bool,
    pub(crate) revealed: Vec<bool>,
}

pub(crate) fn grade(
    matcher_version: u32,
    content: &ThemaContent,
    response: &StructuredResponse,
) -> Result<Grade, GradeError> {
    if matcher_version != MATCHER_VERSION {
        return Err(GradeError::MatcherVersion);
    }
    let success = match (content.kind, response) {
        (ThemaKind::Basic, StructuredResponse::Text(input)) => {
            equivalent(&content.answer[0], input)
        }
        (ThemaKind::Cloze, StructuredResponse::Texts(inputs)) => {
            grade_clozes(&content.answer, inputs)
        }
        (ThemaKind::Mcq | ThemaKind::McCloze, StructuredResponse::Choice(choice)) => {
            grade_choice(content, choice)?
        }
        _ => return Err(GradeError::ResponseShape),
    };
    Ok(Grade {
        matcher_version,
        success,
        revealed_answer: content.answer.clone(),
    })
}

fn grade_clozes(answers: &[String], inputs: &[String]) -> bool {
    check_clozes(answers, inputs).complete
}

pub(crate) fn check_clozes(answers: &[String], inputs: &[String]) -> ClozeCheck {
    let normalized_answers = answers
        .iter()
        .map(|answer| normalize(answer))
        .collect::<Vec<_>>();
    let normalized_inputs = inputs
        .iter()
        .map(|input| normalize(input))
        .collect::<Vec<_>>();
    let adjacency = cloze_adjacency(&normalized_answers, &normalized_inputs);
    let mut answer_owners = vec![None; answers.len()];
    for input_index in 0..inputs.len() {
        let mut visited = vec![false; answers.len()];
        assign_cloze(
            input_index,
            &adjacency.edges,
            &mut answer_owners,
            &mut visited,
        );
    }
    let revealed = answer_owners.iter().map(Option::is_some).collect();
    let mut matches = vec![false; inputs.len()];
    for input_index in answer_owners.into_iter().flatten() {
        matches[input_index] = true;
    }
    let all_entered_correct = matches.iter().all(|matched| *matched);
    ClozeCheck {
        complete: inputs.len() == answers.len() && all_entered_correct,
        matches,
        all_entered_correct,
        revealed,
    }
}

struct ClozeAdjacency {
    edges: Vec<Vec<usize>>,
    #[cfg(test)]
    comparisons: usize,
}

fn cloze_adjacency(answers: &[Vec<char>], inputs: &[Vec<char>]) -> ClozeAdjacency {
    #[cfg(test)]
    let mut comparisons = 0;
    let edges = inputs
        .iter()
        .map(|input| {
            answers
                .iter()
                .enumerate()
                .filter_map(|(answer_index, answer)| {
                    #[cfg(test)]
                    {
                        comparisons += 1;
                    }
                    equivalent_normalized(answer, input).then_some(answer_index)
                })
                .collect()
        })
        .collect();
    ClozeAdjacency {
        edges,
        #[cfg(test)]
        comparisons,
    }
}

fn assign_cloze(
    input_index: usize,
    adjacency: &[Vec<usize>],
    answer_owners: &mut [Option<usize>],
    visited: &mut [bool],
) -> bool {
    for &answer_index in &adjacency[input_index] {
        if visited[answer_index] {
            continue;
        }
        visited[answer_index] = true;
        // Reassign earlier fuzzy matches so input order cannot change the result.
        let available = answer_owners[answer_index]
            .is_none_or(|owner| assign_cloze(owner, adjacency, answer_owners, visited));
        if available {
            answer_owners[answer_index] = Some(input_index);
            return true;
        }
    }
    false
}

fn grade_choice(content: &ThemaContent, choice: &str) -> Result<bool, GradeError> {
    let valid = content
        .hypothesis
        .iter()
        .flatten()
        .any(|candidate| candidate == choice);
    if valid {
        Ok(content.answer[0] == choice)
    } else {
        Err(GradeError::InvalidChoice)
    }
}

fn equivalent(left: &str, right: &str) -> bool {
    let left = normalize(left);
    let right = normalize(right);
    equivalent_normalized(&left, &right)
}

fn equivalent_normalized(left: &[char], right: &[char]) -> bool {
    let maximum = left.len().max(right.len());
    if maximum > 1 {
        distance_at_most_one(left, right)
    } else {
        left == right
    }
}

fn normalize(value: &str) -> Vec<char> {
    let value = value.strip_suffix('"').unwrap_or(value);
    let value = value.strip_prefix('"').unwrap_or(value);
    value
        .chars()
        .filter(|character| !character.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

fn distance_at_most_one(left: &[char], right: &[char]) -> bool {
    if left.len().abs_diff(right.len()) > 1 {
        return false;
    }
    if left.len() == right.len() {
        return left
            .iter()
            .zip(right)
            .filter(|(left, right)| left != right)
            .count()
            <= 1;
    }
    let (shorter, longer) = if left.len() < right.len() {
        (left, right)
    } else {
        (right, left)
    };
    let (mut shorter_index, mut longer_index, mut edits) = (0, 0, 0);
    while shorter_index < shorter.len() && longer_index < longer.len() {
        if shorter[shorter_index] == longer[longer_index] {
            shorter_index += 1;
        } else {
            edits += 1;
            if edits > 1 {
                return false;
            }
        }
        longer_index += 1;
    }
    true
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub(crate) enum GradeError {
    #[error("matcher version is unsupported")]
    MatcherVersion,
    #[error("response does not match the thema kind")]
    ResponseShape,
    #[error("multiple-choice response is not one of the captured choices")]
    InvalidChoice,
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::{GradeError, StructuredResponse, check_clozes, grade};
    use crate::application::thema_content;

    #[test]
    fn basic_matching_preserves_case_whitespace_quotes_and_distance() {
        let content = thema_content(
            "basic",
            "Question".into(),
            vec![],
            vec!["\"Alpha beta\"".into()],
            None,
            None,
            None,
        )
        .unwrap();
        for response in ["alpha beta", "ALPHABETA", "alpha betx"] {
            assert!(
                grade(1, &content, &StructuredResponse::Text(response.into()))
                    .unwrap()
                    .success
            );
        }
        assert!(
            !grade(1, &content, &StructuredResponse::Text("other".into()))
                .unwrap()
                .success
        );
        for (left, right, matches) in [
            ("", "a", false),
            ("a", "ab", true),
            ("ab", "a", true),
            ("ab", "ac", true),
            ("ab", "cd", false),
            ("ab", "ba", false),
        ] {
            assert_eq!(
                super::equivalent(left, right),
                matches,
                "{left:?} {right:?}"
            );
        }
    }

    #[test]
    fn cloze_answers_are_matched_once_in_any_order() {
        let content = thema_content(
            "cloze",
            "Question".into(),
            vec![None, None],
            vec!["alpha".into(), "beta".into()],
            None,
            None,
            None,
        )
        .unwrap();
        let response = StructuredResponse::Texts(vec!["BETA".into(), "alpha".into()]);
        assert!(grade(1, &content, &response).unwrap().success);
    }

    #[test]
    fn cloze_matching_finds_a_valid_order_independent_assignment() {
        let content = thema_content(
            "cloze",
            "Question".into(),
            vec![None, None],
            vec!["cat".into(), "cut".into()],
            None,
            None,
            None,
        )
        .unwrap();
        for inputs in [
            vec!["cot".into(), "bat".into()],
            vec!["bat".into(), "cot".into()],
        ] {
            assert!(
                grade(1, &content, &StructuredResponse::Texts(inputs))
                    .unwrap()
                    .success
            );
        }
    }

    #[test]
    fn partial_cloze_checks_consume_each_correct_answer_once() {
        let answers = vec!["alpha".into(), "beta".into()];

        assert_eq!(
            check_clozes(&answers, &["BETA".into()]),
            super::ClozeCheck {
                matches: vec![true],
                all_entered_correct: true,
                complete: false,
                revealed: vec![false, true],
            }
        );
        assert_eq!(
            check_clozes(&answers, &["alpha".into(), "alpha".into()]).matches,
            vec![true, false]
        );
    }

    #[test]
    fn cloze_matching_normalizes_once_and_bounds_edge_comparisons() {
        let answers = (0..500)
            .map(|index| format!("\"Απάντηση {index:03}\""))
            .collect::<Vec<_>>();
        let inputs = (0..500)
            .map(|index| format!("αΠΑΝΤΗΣΗ{index:03}"))
            .collect::<Vec<_>>();
        let normalized_answers = answers
            .iter()
            .map(|answer| super::normalize(answer))
            .collect::<Vec<_>>();
        let normalized_inputs = inputs
            .iter()
            .map(|input| super::normalize(input))
            .collect::<Vec<_>>();

        let adjacency = super::cloze_adjacency(&normalized_answers, &normalized_inputs);

        assert_eq!(adjacency.comparisons, answers.len() * inputs.len());
        assert!(adjacency.comparisons <= 250_000);
        assert!(check_clozes(&answers, &inputs).complete);
    }

    #[test]
    fn choices_must_be_captured_options() {
        let content = thema_content(
            "mcq",
            "Question".into(),
            vec![Some("a".into()), Some("b".into())],
            vec!["a".into()],
            None,
            None,
            None,
        )
        .unwrap();
        assert!(
            !grade(1, &content, &StructuredResponse::Choice("b".into()))
                .unwrap()
                .success
        );
        assert_eq!(
            grade(1, &content, &StructuredResponse::Choice("c".into())),
            Err(GradeError::InvalidChoice)
        );
    }

    #[test]
    fn matcher_versions_are_explicit() {
        let content = thema_content(
            "basic",
            "Question".into(),
            vec![],
            vec!["answer".into()],
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            grade(2, &content, &StructuredResponse::Text("answer".into())),
            Err(GradeError::MatcherVersion)
        );
    }

    #[derive(Deserialize)]
    struct MatcherFixture {
        matcher: Vec<MatcherCase>,
    }

    #[derive(Deserialize)]
    struct MatcherCase {
        name: String,
        matches: bool,
    }

    #[test]
    fn matcher_matches_live_elisp_parity_vectors() {
        let fixture: MatcherFixture =
            serde_json::from_str(include_str!("../../tests/fixtures/review/parity-v1.json"))
                .unwrap();
        for case in fixture.matcher {
            let (left, right) = match case.name.as_str() {
                "case_whitespace" => (" Answer ", "answer"),
                "quotes" => ("\"answer\"", "answer"),
                "distance_one" => ("answer", "answe"),
                "short_exact" => ("a", "b"),
                _ => panic!("unknown matcher parity case {}", case.name),
            };
            assert_eq!(
                super::equivalent(left, right),
                case.matches,
                "{}",
                case.name
            );
        }
    }
}
