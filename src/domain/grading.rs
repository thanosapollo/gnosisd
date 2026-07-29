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
    let mut answer_owners = vec![None; answers.len()];
    for input_index in 0..inputs.len() {
        let mut visited = vec![false; answers.len()];
        assign_cloze(
            input_index,
            answers,
            inputs,
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

fn assign_cloze(
    input_index: usize,
    answers: &[String],
    inputs: &[String],
    answer_owners: &mut [Option<usize>],
    visited: &mut [bool],
) -> bool {
    for answer_index in 0..answers.len() {
        if visited[answer_index] || !equivalent(&answers[answer_index], &inputs[input_index]) {
            continue;
        }
        visited[answer_index] = true;
        // Reassign earlier fuzzy matches so input order cannot change the result.
        let available = answer_owners[answer_index]
            .is_none_or(|owner| assign_cloze(owner, answers, inputs, answer_owners, visited));
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
    let maximum = left.chars().count().max(right.chars().count());
    if maximum > 1 {
        levenshtein(&left, &right) <= 1
    } else {
        left == right
    }
}

fn normalize(value: &str) -> String {
    let value = value.strip_suffix('"').unwrap_or(value);
    let value = value.strip_prefix('"').unwrap_or(value);
    value
        .chars()
        .filter(|character| !character.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

fn levenshtein(left: &str, right: &str) -> usize {
    let right = right.chars().collect::<Vec<_>>();
    let mut previous = (0..=right.len()).collect::<Vec<_>>();
    for (left_index, left_character) in left.chars().enumerate() {
        let mut current = vec![left_index + 1];
        for (right_index, right_character) in right.iter().enumerate() {
            let substitution =
                previous[right_index] + usize::from(left_character != *right_character);
            current.push(
                substitution
                    .min(current[right_index] + 1)
                    .min(previous[right_index + 1] + 1),
            );
        }
        previous = current;
    }
    previous[right.len()]
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
        let response = StructuredResponse::Texts(vec!["cot".into(), "bat".into()]);

        assert!(grade(1, &content, &response).unwrap().success);
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
