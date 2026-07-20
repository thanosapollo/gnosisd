use std::{env, fs, path::PathBuf};

use serde::{Deserialize, Serialize};

use super::org::{OrgParseError, parse_org};
use crate::domain::{DocumentProjection, Node};

#[derive(Debug, Deserialize)]
struct ExpectedProjection {
    title: String,
    file_node_id: Option<String>,
    file_tags: Vec<String>,
    nodes: Vec<ExpectedNode>,
    link_occurrences: Vec<ExpectedOccurrence>,
    semantic_edges: Vec<ExpectedEdge>,
    content_sha256: String,
}

#[derive(Debug, Deserialize)]
struct ExpectedNode {
    id: String,
    title: String,
    title_path: Vec<String>,
    tags: Vec<String>,
    parent_id: Option<String>,
    level: u32,
}

#[derive(Debug, Deserialize)]
struct ExpectedOccurrence {
    owner: String,
    target: String,
    start: usize,
    end: usize,
}

#[derive(Debug, Deserialize)]
struct ExpectedEdge {
    source: String,
    target: String,
}

#[derive(Debug, Serialize)]
struct SharedProjection<'a> {
    title: &'a str,
    file_node_id: Option<&'a str>,
    file_tags: &'a [String],
    nodes: Vec<SharedNode<'a>>,
    link_occurrences: Vec<SharedOccurrence<'a>>,
    semantic_edges: Vec<SharedEdge<'a>>,
}

#[derive(Debug, Serialize)]
struct SharedNode<'a> {
    id: &'a str,
    title: &'a str,
    title_path: &'a [String],
    tags: &'a [String],
    parent_id: Option<&'a str>,
    level: u32,
}

#[derive(Debug, Serialize)]
struct SharedOccurrence<'a> {
    owner: &'a str,
    target: &'a str,
}

#[derive(Debug, Serialize)]
struct SharedEdge<'a> {
    source: &'a str,
    target: &'a str,
}

#[test]
fn projects_basic_document() {
    assert_fixture("basic");
}

#[test]
fn skips_unowned_links_and_unindexed_ancestors() {
    assert_fixture("no-file-id");
}

#[test]
fn preserves_duplicate_occurrences_but_deduplicates_edges() {
    assert_fixture("duplicate-links");
}

#[test]
fn reports_byte_spans_for_unicode_input() {
    assert_fixture("unicode");
}

#[test]
fn matches_literal_links_todo_keywords_and_comment_titles() {
    assert_fixture("compatibility");
}

#[test]
fn rejects_missing_document_title() {
    let input = fixture_text("missing-title");
    assert_eq!(parse_org(&input), Err(OrgParseError::MissingTitle));
}

#[test]
fn rejects_blank_indexed_heading_title() {
    let input = fixture_text("blank-title");
    let error = parse_org(&input).unwrap_err();
    assert!(matches!(error, OrgParseError::InvalidProjection(_)));
}

#[test]
fn parsing_is_deterministic() {
    let input = fixture_text("basic");
    assert_eq!(parse_org(&input), parse_org(&input));
}

#[test]
fn leaves_bare_title_links_intact() {
    let projection =
        parse_org("#+TITLE: Bare [[id:a]] then [[id:b][B]]\n").expect("title must parse");
    assert_eq!(projection.title(), "Bare [[id:a]] then B");
}

#[test]
fn ignores_drawer_text_in_literal_blocks() {
    let input = "#+TITLE: No root\n#+begin_src org\n:PROPERTIES:\n:ID: fake\n:END:\n#+end_src\n";
    let projection = parse_org(input).expect("document must parse");
    assert!(projection.file_node_id().is_none());
}

#[test]
fn accepts_empty_quote_blocks_without_changing_source_offsets() {
    let input = concat!(
        ":PROPERTIES:\n:ID: root\n:END:\n#+TITLE: Quotes\n\n",
        "#+BEGIN_QUOTE\n\n#+END_QUOTE\n",
        "See [[id:target]].\n",
    );
    let projection = parse_org(input).expect("document must parse");
    let occurrence = projection.link_occurrences().first().unwrap();

    assert_eq!(
        input.get(occurrence.span().start()..occurrence.span().end()),
        Some("[[id:target]]")
    );
}

#[test]
fn does_not_take_file_id_from_a_later_drawer() {
    let input = concat!(
        ":PROPERTIES:\n:OWNER: someone\n:END:\n#+TITLE: No root\n\n",
        "Later text\n:PROPERTIES:\n:ID: not-the-root\n:END:\n",
    );
    let projection = parse_org(input).expect("document must parse");
    assert!(projection.file_node_id().is_none());
}

#[test]
fn does_not_take_file_id_when_there_is_no_leading_drawer() {
    let input = concat!(
        "#+TITLE: No root\nLater text\n",
        ":PROPERTIES:\n:ID: not-the-root\n:END:\n",
    );
    let projection = parse_org(input).expect("document must parse");
    assert!(projection.file_node_id().is_none());
}

#[test]
fn accepts_file_drawer_after_leading_comments() {
    let input = concat!(
        "# Comment\n\n# Another comment\n",
        ":PROPERTIES:\n:ID: root\n:END:\n#+TITLE: Root\n",
    );
    let projection = parse_org(input).expect("document must parse");
    assert_eq!(
        projection.file_node_id().map(|id| id.as_str()),
        Some("root")
    );
}

#[test]
fn rejects_file_drawer_after_an_indented_comment() {
    let input = concat!(
        "# Comment\n\n  # Indented comment\n",
        ":PROPERTIES:\n:ID: root\n:END:\n#+TITLE: Root\n",
    );
    let projection = parse_org(input).expect("document must parse");
    assert!(projection.file_node_id().is_none());
}

#[test]
fn plain_links_require_boundaries_and_drop_trailing_punctuation() {
    let input = concat!(
        ":PROPERTIES:\n:ID: root\n:END:\n#+TITLE: Plain\n\n",
        "invalid:wrong -id:dash id:foo/bar id:foo:bar id:right.\n",
    );
    let projection = parse_org(input).expect("document must parse");
    let targets = link_targets(&projection);
    assert_eq!(targets, ["dash", "foo/bar", "foo:bar", "right"]);
    let occurrence = projection.link_occurrences().last().unwrap();
    assert_eq!(
        input.get(occurrence.span().start()..occurrence.span().end()),
        Some("id:right")
    );
}

#[test]
fn plain_links_match_org_trailing_rules() {
    let input = concat!(
        ":PROPERTIES:\n:ID: root\n:END:\n#+TITLE: Plain\n\n",
        "id:a.b id:d/ id:g- id:u_ id:t~ id:h# id:e! id:q? id:c: id:foo.\n",
    );
    let projection = parse_org(input).expect("document must parse");
    assert_eq!(link_targets(&projection), ["a.b", "d/", "g-", "foo"]);
}

#[test]
fn plain_links_include_balanced_parentheses() {
    let input = concat!(
        ":PROPERTIES:\n:ID: root\n:END:\n#+TITLE: Plain\n\n",
        "id:foo(bar) id:nested(one(two)) id:open(broken id:closed)\n",
    );
    let projection = parse_org(input).expect("document must parse");
    assert_eq!(
        link_targets(&projection),
        ["foo(bar)", "nested(one(two))", "open", "closed"]
    );
    let first = projection.link_occurrences()[0].span();
    assert_eq!(input.get(first.start()..first.end()), Some("id:foo(bar)"));
}

#[test]
fn plain_links_trim_trailing_punctuation_after_two_characters() {
    let input = concat!(
        ":PROPERTIES:\n:ID: root\n:END:\n#+TITLE: Plain\n\n",
        "id:a id:foo_ id:foo~ id:foo# id:foo@ id:foo% id:foo+ ",
        "id:foo= id:foo\\ id:foo? id:foo! id:foo:\n",
    );
    let projection = parse_org(input).expect("document must parse");
    assert_eq!(link_targets(&projection), ["foo"; 11]);
}

#[test]
fn malformed_bracket_link_does_not_consume_the_next_link() {
    let input = concat!(
        ":PROPERTIES:\n:ID: root\n:END:\n#+TITLE: Links\n\n",
        "Broken [[id:bad\nGood [[id:good]]\n",
    );
    let projection = parse_org(input).expect("document must parse");
    assert_eq!(link_targets(&projection), ["bad", "good"]);
}

#[test]
fn malformed_bracket_wrapper_falls_back_to_plain_link() {
    let input = concat!(
        ":PROPERTIES:\n:ID: root\n:END:\n#+TITLE: Links\n\n",
        "Broken [[id:foo] garbage ]] then [[id:good]]\n",
    );
    let projection = parse_org(input).expect("document must parse");
    assert_eq!(link_targets(&projection), ["foo", "good"]);
    let span = projection.link_occurrences()[0].span();
    assert_eq!(input.get(span.start()..span.end()), Some("id:foo"));
}

#[test]
#[ignore = "development oracle invoked by tools/check-org-parity.sh"]
fn export_parity_projection() {
    let fixture = env::var("GNOSIS_PARITY_FIXTURE").expect("fixture path is required");
    let output = env::var("GNOSIS_PARITY_OUTPUT").expect("output path is required");
    let input = fs::read_to_string(fixture).expect("fixture must be readable UTF-8");
    let projection = parse_org(&input).expect("fixture must parse");
    let json = serde_json::to_vec(&shared_projection(&projection)).expect("projection serializes");
    fs::write(output, json).expect("parity output must be writable");
}

fn link_targets(projection: &DocumentProjection) -> Vec<&str> {
    projection
        .link_occurrences()
        .iter()
        .map(|occurrence| occurrence.target().as_str())
        .collect()
}

fn assert_fixture(name: &str) {
    let input = fixture_text(name);
    let expected: ExpectedProjection =
        serde_json::from_str(&fixture_json(name)).expect("fixture JSON must be valid");
    let actual = parse_org(&input).expect("fixture must parse");

    assert_eq!(actual.title(), expected.title);
    assert_eq!(
        actual.file_node_id().map(|id| id.as_str()),
        expected.file_node_id.as_deref()
    );
    assert_eq!(actual.file_tags(), expected.file_tags);
    assert_nodes(&actual, &expected);
    assert_occurrences(&input, &actual, &expected);
    assert_edges(&actual, &expected);
    assert_eq!(actual.content_sha256(), expected.content_sha256);
}

fn assert_nodes(actual: &DocumentProjection, expected: &ExpectedProjection) {
    assert_eq!(actual.nodes().len(), expected.nodes.len());
    for (actual, expected) in actual.nodes().iter().zip(&expected.nodes) {
        assert_eq!(actual.id().as_str(), expected.id);
        assert_eq!(actual.title(), expected.title);
        assert_eq!(actual.title_path().parts(), expected.title_path);
        assert_eq!(actual.tags(), expected.tags);
        assert_eq!(
            actual.parent_id().map(|id| id.as_str()),
            expected.parent_id.as_deref()
        );
        assert_eq!(actual.level(), expected.level);
    }
}

fn assert_occurrences(input: &str, actual: &DocumentProjection, expected: &ExpectedProjection) {
    assert_eq!(
        actual.link_occurrences().len(),
        expected.link_occurrences.len()
    );
    for (actual, expected) in actual
        .link_occurrences()
        .iter()
        .zip(&expected.link_occurrences)
    {
        assert_eq!(actual.owner().as_str(), expected.owner);
        assert_eq!(actual.target().as_str(), expected.target);
        assert_eq!(actual.span().start(), expected.start);
        assert_eq!(actual.span().end(), expected.end);
        assert!(input.get(expected.start..expected.end).is_some());
    }
}

fn assert_edges(actual: &DocumentProjection, expected: &ExpectedProjection) {
    assert_eq!(actual.semantic_edges().len(), expected.semantic_edges.len());
    for (actual, expected) in actual.semantic_edges().iter().zip(&expected.semantic_edges) {
        assert_eq!(actual.source().as_str(), expected.source);
        assert_eq!(actual.target().as_str(), expected.target);
    }
}

fn fixture_path(name: &str, extension: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/org")
        .join(format!("{name}.{extension}"))
}

fn fixture_text(name: &str) -> String {
    fs::read_to_string(fixture_path(name, "org")).expect("fixture must be readable UTF-8")
}

fn fixture_json(name: &str) -> String {
    fs::read_to_string(fixture_path(name, "json")).expect("fixture JSON must be readable")
}

fn shared_projection(projection: &DocumentProjection) -> SharedProjection<'_> {
    SharedProjection {
        title: projection.title(),
        file_node_id: projection.file_node_id().map(|id| id.as_str()),
        file_tags: projection.file_tags(),
        nodes: projection.nodes().iter().map(shared_node).collect(),
        link_occurrences: projection
            .link_occurrences()
            .iter()
            .map(|link| SharedOccurrence {
                owner: link.owner().as_str(),
                target: link.target().as_str(),
            })
            .collect(),
        semantic_edges: projection
            .semantic_edges()
            .iter()
            .map(|edge| SharedEdge {
                source: edge.source().as_str(),
                target: edge.target().as_str(),
            })
            .collect(),
    }
}

fn shared_node(node: &Node) -> SharedNode<'_> {
    SharedNode {
        id: node.id().as_str(),
        title: node.title(),
        title_path: node.title_path().parts(),
        tags: node.tags(),
        parent_id: node.parent_id().map(|id| id.as_str()),
        level: node.level(),
    }
}
