use std::{borrow::Cow, fmt};

use orgize::{
    Event, Org, ParseConfig,
    elements::{Element, Title},
};
use sha1::{Digest as _, Sha1};
use sha2::Sha256;
use thiserror::Error;

use crate::domain::{
    DocumentProjection, DocumentProjectionInput, IdError, LinkOccurrence, Node, NodeId, SourceSpan,
    TitlePath,
};

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum OrgParseError {
    #[error("document has no non-empty title")]
    MissingTitle,
    #[error("invalid Org projection: {0}")]
    InvalidProjection(String),
    #[error("headline level {0} does not fit the domain model")]
    InvalidHeadlineLevel(usize),
    #[error(transparent)]
    InvalidId(#[from] IdError),
}

#[derive(Clone, Debug)]
struct HeadingContext {
    level: usize,
    owner: Option<NodeId>,
    title_path: Vec<String>,
    tags: Vec<String>,
}

#[derive(Clone, Debug)]
struct HeadingBoundary {
    start: usize,
    level: usize,
    owner: Option<NodeId>,
}

struct ProjectionBuilder<'a> {
    input: &'a str,
    file_node_id: Option<NodeId>,
    file_title_path: Vec<String>,
    file_tags: Vec<String>,
    headings: Vec<HeadingContext>,
    heading_boundaries: Vec<HeadingBoundary>,
    nodes: Vec<Node>,
}

pub fn parse_org(input: &str) -> Result<DocumentProjection, OrgParseError> {
    let config = parse_config(input);
    let parser_input = neutralize_empty_quote_blocks(input);
    let org = Org::parse_custom(parser_input.as_ref(), &config);
    let title = document_title(&org)?;
    let file_tags = document_tags(&org);
    let file_node_id = file_id(input, &org).map(NodeId::new).transpose()?;
    let boundaries = heading_boundaries(input, &org)?;
    let mut builder =
        ProjectionBuilder::new(input, &title, file_node_id.clone(), &file_tags, boundaries)?;

    for event in org.iter() {
        builder.process_event(event)?;
    }

    builder.finish(title, file_node_id, file_tags)
}

fn neutralize_empty_quote_blocks(input: &str) -> Cow<'_, str> {
    let mut output = input.to_owned();
    let lines = input.split_inclusive('\n').collect::<Vec<_>>();
    let mut offset = 0;
    let mut begin = None;
    for line in lines {
        let trimmed = line.trim();
        if trimmed.eq_ignore_ascii_case("#+begin_quote") {
            begin = Some((offset, offset + line.len()));
        } else if trimmed.eq_ignore_ascii_case("#+end_quote") {
            if let Some((start, body_start)) = begin.take()
                && input[body_start..offset].trim().is_empty()
            {
                neutralize_marker(&mut output, start, body_start);
                neutralize_marker(&mut output, offset, offset + line.len());
            }
        } else if begin.is_some() && !trimmed.is_empty() {
            begin = None;
        }
        offset += line.len();
    }
    if output == input {
        Cow::Borrowed(input)
    } else {
        Cow::Owned(output)
    }
}

fn neutralize_marker(input: &mut String, start: usize, end: usize) {
    if let Some(index) = input[start..end].find('+') {
        let index = start + index;
        input.replace_range(index..index + 1, " ");
    }
}

impl<'a> ProjectionBuilder<'a> {
    fn new(
        input: &'a str,
        title: &str,
        file_node_id: Option<NodeId>,
        file_tags: &[String],
        heading_boundaries: Vec<HeadingBoundary>,
    ) -> Result<Self, OrgParseError> {
        let mut nodes = Vec::new();
        let file_title_path = file_node_id
            .as_ref()
            .map(|_| vec![title.to_owned()])
            .unwrap_or_default();
        if let Some(id) = &file_node_id {
            nodes.push(make_node(
                id.clone(),
                title.to_owned(),
                file_title_path.clone(),
                file_tags.to_vec(),
                None,
                0,
            )?);
        }
        Ok(Self {
            input,
            file_node_id,
            file_title_path,
            file_tags: file_tags.to_vec(),
            headings: Vec::new(),
            heading_boundaries,
            nodes,
        })
    }

    fn process_event(&mut self, event: Event<'_, '_>) -> Result<(), OrgParseError> {
        match event {
            Event::Start(Element::Headline { level }) => self.start_headline(*level),
            Event::Start(Element::Title(title)) => self.start_title(title)?,
            Event::End(Element::Headline { .. }) => {
                self.headings.pop();
            }
            _ => {}
        }
        Ok(())
    }

    fn start_headline(&mut self, level: usize) {
        self.headings.push(HeadingContext {
            level,
            owner: self.current_owner(),
            title_path: self.current_title_path(),
            tags: self.current_tags(),
        });
    }

    fn start_title(&mut self, title: &Title<'_>) -> Result<(), OrgParseError> {
        let Some(context) = self.headings.last_mut() else {
            return Ok(());
        };
        context.tags = combine_tags(&context.tags, title.tags.iter().map(AsRef::as_ref));
        let Some(id) = property(title, "ID") else {
            return Ok(());
        };
        let id = NodeId::new(id)?;
        let local_title = adjust_title(comment_free_title(title));
        let mut title_path = context.title_path.clone();
        title_path.push(local_title.clone());
        let level = u32::try_from(context.level)
            .map_err(|_| OrgParseError::InvalidHeadlineLevel(context.level))?;
        self.nodes.push(make_node(
            id.clone(),
            local_title,
            title_path.clone(),
            context.tags.clone(),
            context.owner.clone(),
            level,
        )?);
        context.owner = Some(id);
        context.title_path = title_path;
        Ok(())
    }

    fn current_owner(&self) -> Option<NodeId> {
        self.headings
            .last()
            .and_then(|heading| heading.owner.clone())
            .or_else(|| self.file_node_id.clone())
    }

    fn current_title_path(&self) -> Vec<String> {
        self.headings
            .last()
            .map(|heading| heading.title_path.clone())
            .unwrap_or_else(|| self.file_title_path.clone())
    }

    fn current_tags(&self) -> Vec<String> {
        self.headings
            .last()
            .map(|heading| heading.tags.clone())
            .unwrap_or_else(|| self.file_tags.clone())
    }

    fn finish(
        self,
        title: String,
        file_node_id: Option<NodeId>,
        file_tags: Vec<String>,
    ) -> Result<DocumentProjection, OrgParseError> {
        let link_occurrences =
            locate_links(self.input, &self.heading_boundaries, file_node_id.as_ref())?;
        let input = DocumentProjectionInput {
            title,
            file_node_id,
            file_tags,
            nodes: self.nodes,
            link_occurrences,
            compatibility_sha1: format!("{:x}", Sha1::digest(self.input.as_bytes())),
            content_sha256: format!("{:x}", Sha256::digest(self.input.as_bytes())),
        };
        DocumentProjection::new(input).map_err(invalid_projection)
    }
}

fn document_title(org: &Org<'_>) -> Result<String, OrgParseError> {
    org.keywords()
        .find(|keyword| keyword.key.eq_ignore_ascii_case("TITLE"))
        .map(|keyword| adjust_title(&keyword.value))
        .filter(|title| !title.trim().is_empty())
        .ok_or(OrgParseError::MissingTitle)
}

fn document_tags(org: &Org<'_>) -> Vec<String> {
    org.keywords()
        .find(|keyword| keyword.key.eq_ignore_ascii_case("FILETAGS"))
        .map(|keyword| combine_tags(&[], keyword.value.split(':')))
        .unwrap_or_default()
}

fn parse_config(input: &str) -> ParseConfig {
    let mut in_block = false;
    let sequences = input.lines().filter_map(|line| {
        let trimmed = line.trim_start();
        if starts_with_ignore_case(trimmed, "#+begin_") {
            in_block = true;
            None
        } else if starts_with_ignore_case(trimmed, "#+end_") {
            in_block = false;
            None
        } else if in_block {
            None
        } else {
            todo_keyword_value(line)
        }
    });
    let sequences = sequences.collect::<Vec<_>>();
    if sequences.is_empty() {
        return ParseConfig::default();
    }
    let (active, done) = sequences.into_iter().flat_map(todo_sequence).fold(
        (Vec::new(), Vec::new()),
        |(mut active, mut done), (keyword, completed)| {
            if completed {
                done.push(keyword);
            } else {
                active.push(keyword);
            }
            (active, done)
        },
    );
    ParseConfig {
        todo_keywords: (active, done),
    }
}

fn todo_keyword_value(line: &str) -> Option<&str> {
    let (name, value) = line.trim_start().split_once(':')?;
    ["#+TODO", "#+SEQ_TODO", "#+TYP_TODO"]
        .iter()
        .any(|keyword| name.eq_ignore_ascii_case(keyword))
        .then_some(value.trim())
}

fn todo_sequence(value: &str) -> Vec<(String, bool)> {
    let mut completed = false;
    value
        .split_ascii_whitespace()
        .filter_map(|token| {
            if token == "|" {
                completed = true;
                None
            } else {
                Some((token.split('(').next()?.to_owned(), completed))
            }
        })
        .collect()
}

fn file_id(input: &str, org: &Org<'_>) -> Option<String> {
    if !has_file_properties_drawer(org) {
        return None;
    }
    let mut lines = input
        .lines()
        .skip_while(|line| line.trim().is_empty() || is_org_comment_line(line));
    if lines.next().map(str::trim) != Some(":PROPERTIES:") {
        return None;
    }
    for line in lines {
        if line.trim() == ":END:" {
            return None;
        }
        if let Some(id) = line.trim().strip_prefix(":ID:").map(str::trim) {
            return (!id.is_empty()).then(|| id.to_owned());
        }
    }
    None
}

fn is_org_comment_line(line: &str) -> bool {
    line.strip_prefix('#')
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
}

fn has_file_properties_drawer(org: &Org<'_>) -> bool {
    org.document().section_node().is_some_and(|section| {
        section.children(org.arena()).any(|node| {
            matches!(
                &org[node],
                Element::Drawer(drawer) if drawer.name == "PROPERTIES"
            )
        })
    })
}

fn is_headline(line: &str) -> bool {
    let stars = line.bytes().take_while(|byte| *byte == b'*').count();
    stars > 0
        && matches!(
            line.as_bytes().get(stars),
            Some(b' ' | b'\t' | b'\r' | b'\n') | None
        )
}

fn heading_boundaries(input: &str, org: &Org<'_>) -> Result<Vec<HeadingBoundary>, OrgParseError> {
    let starts = headline_starts(input);
    let headlines = org.headlines().collect::<Vec<_>>();
    if starts.len() != headlines.len() {
        return Err(OrgParseError::InvalidProjection(
            "could not align parsed headlines with source".to_owned(),
        ));
    }
    starts
        .into_iter()
        .zip(headlines)
        .map(|(start, headline)| {
            let title = headline.title(org);
            Ok(HeadingBoundary {
                start,
                level: headline.level(),
                owner: property(title, "ID").map(NodeId::new).transpose()?,
            })
        })
        .collect()
}

fn headline_starts(input: &str) -> Vec<usize> {
    let mut offset = 0;
    let mut in_block = false;
    input
        .split_inclusive('\n')
        .filter_map(|line| {
            let start = offset;
            offset += line.len();
            let trimmed = line.trim_start();
            if starts_with_ignore_case(trimmed, "#+begin_") {
                in_block = true;
                None
            } else if starts_with_ignore_case(trimmed, "#+end_") {
                in_block = false;
                None
            } else {
                (!in_block && is_headline(line)).then_some(start)
            }
        })
        .collect()
}

fn starts_with_ignore_case(input: &str, prefix: &str) -> bool {
    input
        .get(..prefix.len())
        .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
}

fn property<'a>(title: &'a Title<'_>, key: &str) -> Option<&'a str> {
    title
        .properties
        .iter()
        .find(|(name, _)| name.as_ref() == key)
        .map(|(_, value)| value.as_ref())
        .filter(|value| !value.trim().is_empty())
}

fn comment_free_title<'a>(title: &'a Title<'_>) -> &'a str {
    title
        .is_commented()
        .then(|| title.raw.strip_prefix("COMMENT").map(str::trim_start))
        .flatten()
        .unwrap_or(&title.raw)
}

fn make_node(
    id: NodeId,
    title: String,
    title_path: Vec<String>,
    tags: Vec<String>,
    parent_id: Option<NodeId>,
    level: u32,
) -> Result<Node, OrgParseError> {
    let title_path = TitlePath::new(title_path).map_err(invalid_projection)?;
    Node::new(id, title, title_path, tags, parent_id, level).map_err(invalid_projection)
}

fn combine_tags<'a>(inherited: &[String], added: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    added
        .into_iter()
        .filter(|tag| !tag.is_empty())
        .fold(inherited.to_vec(), |mut tags, tag| {
            if !tags.iter().any(|existing| existing == tag) {
                tags.push(tag.to_owned());
            }
            tags
        })
}

fn adjust_title(input: &str) -> String {
    let mut adjusted = String::new();
    let mut rest = input;
    while let Some(start) = rest.find("[[id:") {
        adjusted.push_str(&rest[..start]);
        let link = &rest[start..];
        let Some(end) = link.find("]]") else {
            adjusted.push_str(link);
            return adjusted.trim().to_owned();
        };
        let source = &link[..end + 2];
        if let Some(description_start) = source.find("][") {
            adjusted.push_str(&source[description_start + 2..source.len() - 2]);
        } else {
            adjusted.push_str(source);
        }
        rest = &link[end + 2..];
    }
    adjusted.push_str(rest);
    adjusted.trim().to_owned()
}

fn locate_links(
    input: &str,
    boundaries: &[HeadingBoundary],
    file_node_id: Option<&NodeId>,
) -> Result<Vec<LinkOccurrence>, OrgParseError> {
    let mut boundary_index = 0;
    let mut headings = Vec::new();
    let mut occurrences = Vec::new();
    for (target, span) in raw_id_links(input)? {
        while boundaries
            .get(boundary_index)
            .is_some_and(|boundary| boundary.start <= span.start())
        {
            let boundary = boundaries[boundary_index].clone();
            while headings
                .last()
                .is_some_and(|current: &HeadingBoundary| current.level >= boundary.level)
            {
                headings.pop();
            }
            headings.push(boundary);
            boundary_index += 1;
        }
        let owner = headings
            .iter()
            .rev()
            .find_map(|heading| heading.owner.clone())
            .or_else(|| file_node_id.cloned());
        if let Some(owner) = owner {
            occurrences.push(LinkOccurrence::new(owner, NodeId::new(target)?, span));
        }
    }
    Ok(occurrences)
}

fn raw_id_links(input: &str) -> Result<Vec<(String, SourceSpan)>, OrgParseError> {
    let mut cursor = 0;
    let mut links = Vec::new();
    while let Some(relative) = input[cursor..].find("id:") {
        let scheme_start = cursor + relative;
        if !valid_id_scheme_boundary(input, scheme_start) {
            cursor = scheme_start + 3;
            continue;
        }
        let Some((target, start, end)) = raw_id_link_at(input, scheme_start) else {
            cursor = scheme_start + 3;
            continue;
        };
        if !target.is_empty() {
            links.push((
                target.to_owned(),
                SourceSpan::for_source(input, start, end).map_err(invalid_projection)?,
            ));
        }
        cursor = end;
    }
    Ok(links)
}

fn valid_id_scheme_boundary(input: &str, scheme_start: usize) -> bool {
    if input.get(scheme_start.saturating_sub(2)..scheme_start) == Some("[[")
        || input.get(scheme_start.saturating_sub(1)..scheme_start) == Some("<")
    {
        return true;
    }
    input
        .get(..scheme_start)
        .and_then(|prefix| prefix.chars().next_back())
        .is_none_or(|character| !character.is_alphanumeric())
}

fn raw_id_link_at(input: &str, scheme_start: usize) -> Option<(&str, usize, usize)> {
    let target_start = scheme_start + 3;
    if input.get(scheme_start.saturating_sub(2)..scheme_start) == Some("[[") {
        let line = input.get(scheme_start..)?.split('\n').next()?;
        if let Some(target_end) = line.find(']') {
            let suffix = &line[target_end..];
            let wrapped_end = if suffix.starts_with("]]") {
                Some(target_end + 2)
            } else if suffix.starts_with("][") {
                suffix.find("]]").map(|end| target_end + end + 2)
            } else {
                None
            };
            if let Some(end) = wrapped_end {
                return Some((&line[3..target_end], scheme_start - 2, scheme_start + end));
            }
        }
    } else if input.get(scheme_start.saturating_sub(1)..scheme_start) == Some("<") {
        let line = input.get(target_start..)?.split('\n').next()?;
        if let Some(relative_end) = line.find('>') {
            return Some((
                &line[..relative_end],
                scheme_start - 1,
                target_start + relative_end + 1,
            ));
        }
    }
    plain_id_link(input, scheme_start)
}

fn plain_id_link(input: &str, scheme_start: usize) -> Option<(&str, usize, usize)> {
    let target_start = scheme_start + 3;
    let rest = input.get(target_start..)?;
    let length = plain_link_target_length(rest);
    let target = &rest[..length];
    (target.chars().count() >= 2).then_some((target, scheme_start, target_start + target.len()))
}

fn plain_link_target_length(input: &str) -> usize {
    let mut parentheses = Vec::new();
    let mut valid_end = 0;

    for (offset, character) in input.char_indices() {
        if plain_link_delimiter(character) {
            break;
        }
        let end = offset + character.len_utf8();
        match character {
            '(' => parentheses.push(offset),
            ')' => {
                if parentheses.pop().is_none() {
                    break;
                }
                if parentheses.is_empty() {
                    valid_end = end;
                }
            }
            _ if parentheses.is_empty()
                && (character.is_alphanumeric() || character == '/' || character == '-') =>
            {
                valid_end = end;
            }
            _ => {}
        }
    }
    valid_end
}

fn plain_link_delimiter(character: char) -> bool {
    character.is_whitespace() || "[]<>{}\\\"'".contains(character)
}

fn invalid_projection(error: impl fmt::Display) -> OrgParseError {
    OrgParseError::InvalidProjection(error.to_string())
}
