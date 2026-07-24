use std::ops::Range;

use local_rpc::model::{CommandArgKind, CommandCandidate, CommandCandidateKind, CommandInfo};

pub const MAX_VISIBLE_OPTIONS: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompletionContext {
    Command {
        query: String,
        span: Range<usize>,
    },
    Argument {
        command: CommandInfo,
        kind: ArgumentKind,
        query: String,
        span: Range<usize>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArgumentKind {
    Candidates(CommandCandidateKind),
    Free,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum OptionKey {
    Command(String),
    Candidate {
        kind: CommandCandidateKind,
        value: String,
        detail: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompletionValue {
    Command(CommandInfo),
    Candidate {
        kind: CommandCandidateKind,
        item: CommandCandidate,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionOption {
    pub key: OptionKey,
    pub value: CompletionValue,
    pub match_ranges: Vec<Range<usize>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Replacement {
    pub span: Range<usize>,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssistSession {
    pub context_key: String,
    pub active: Option<OptionKey>,
    pub engaged: bool,
}

pub fn completion_context(
    text: &str,
    selection: Range<usize>,
    accepts_completion: bool,
    composing: bool,
    commands: &[CommandInfo],
) -> Option<CompletionContext> {
    if !accepts_completion
        || composing
        || selection.start != selection.end
        || selection.end > text.len()
        || !text.is_char_boundary(selection.end)
        || !text.starts_with('/')
    {
        return None;
    }

    let cursor = selection.end;
    let token_end = text.find(char::is_whitespace).unwrap_or(text.len());
    if cursor <= token_end {
        return Some(CompletionContext::Command {
            query: text[..cursor].to_string(),
            span: 0..token_end,
        });
    }

    let name = &text[..token_end];
    let command = commands
        .iter()
        .find(|command| command.name == name)?
        .clone();
    let kind = match command.arg {
        CommandArgKind::None => return None,
        CommandArgKind::User => ArgumentKind::Candidates(CommandCandidateKind::User),
        CommandArgKind::Room => ArgumentKind::Candidates(CommandCandidateKind::Room),
        CommandArgKind::Sound => ArgumentKind::Candidates(CommandCandidateKind::Sound),
        CommandArgKind::Free => ArgumentKind::Free,
    };
    let argument_start = token_end + text[token_end..].chars().next()?.len_utf8();
    if cursor < argument_start {
        return None;
    }
    Some(CompletionContext::Argument {
        command,
        kind,
        query: text[argument_start..cursor].to_string(),
        span: argument_start..text.len(),
    })
}

pub fn context_key(context: &CompletionContext) -> String {
    match context {
        CompletionContext::Command { span, .. } => format!("command:{}", span.start),
        CompletionContext::Argument {
            command,
            kind,
            span,
            ..
        } => format!("argument:{}:{kind:?}:{}", command.name, span.start),
    }
}

pub fn command_options(commands: &[CommandInfo], query: &str) -> Vec<CompletionOption> {
    let mut rows = commands
        .iter()
        .filter_map(|command| {
            let matched = fuzzy_match(query, &command.name)?;
            Some((
                matched.score,
                command.name.to_lowercase(),
                CompletionOption {
                    key: OptionKey::Command(command.name.clone()),
                    value: CompletionValue::Command(command.clone()),
                    match_ranges: matched.ranges,
                },
            ))
        })
        .collect::<Vec<_>>();
    rows.sort_by(|left, right| {
        right
            .0
            .cmp(&left.0)
            .then_with(|| left.1.cmp(&right.1))
            .then_with(|| option_label(&left.2).cmp(option_label(&right.2)))
    });
    rows.into_iter()
        .take(MAX_VISIBLE_OPTIONS)
        .map(|(_, _, option)| option)
        .collect()
}

pub fn candidate_options(
    kind: CommandCandidateKind,
    candidates: &[CommandCandidate],
    query: &str,
) -> Vec<CompletionOption> {
    let mut rows = candidates
        .iter()
        .filter_map(|item| {
            let matched = fuzzy_match(query, &item.value)?;
            Some((
                matched.score,
                item.value.to_lowercase(),
                CompletionOption {
                    key: OptionKey::Candidate {
                        kind,
                        value: item.value.clone(),
                        detail: item.detail.clone(),
                    },
                    value: CompletionValue::Candidate {
                        kind,
                        item: item.clone(),
                    },
                    match_ranges: matched.ranges,
                },
            ))
        })
        .collect::<Vec<_>>();
    rows.sort_by(|left, right| {
        right
            .0
            .cmp(&left.0)
            .then_with(|| left.1.cmp(&right.1))
            .then_with(|| option_label(&left.2).cmp(option_label(&right.2)))
    });
    rows.into_iter()
        .take(MAX_VISIBLE_OPTIONS)
        .map(|(_, _, option)| option)
        .collect()
}

pub fn option_label(option: &CompletionOption) -> &str {
    match &option.value {
        CompletionValue::Command(command) => &command.name,
        CompletionValue::Candidate { item, .. } => &item.value,
    }
}

pub fn replacement(context: &CompletionContext, option: &CompletionOption) -> Replacement {
    let text = match &option.value {
        CompletionValue::Command(command) if command.arg != CommandArgKind::None => {
            format!("{} ", command.name)
        }
        CompletionValue::Command(command) => command.name.clone(),
        CompletionValue::Candidate { item, .. } => item.value.clone(),
    };
    let span = match context {
        CompletionContext::Command { span, .. } | CompletionContext::Argument { span, .. } => {
            span.clone()
        }
    };
    Replacement { span, text }
}

pub fn open_session(context: &CompletionContext) -> AssistSession {
    AssistSession {
        context_key: context_key(context),
        active: None,
        engaged: false,
    }
}

pub fn reconcile_session(
    session: &mut Option<AssistSession>,
    context: Option<&CompletionContext>,
    options: &[CompletionOption],
) {
    let Some(context) = context else {
        *session = None;
        return;
    };
    let key = context_key(context);
    let Some(current) = session else {
        return;
    };
    if current.context_key != key {
        *session = None;
        return;
    }
    if current
        .active
        .as_ref()
        .is_some_and(|active| !options.iter().any(|option| &option.key == active))
    {
        current.active = None;
        current.engaged = false;
    }
}

pub fn move_selection(
    session: &mut AssistSession,
    options: &[CompletionOption],
    delta: isize,
) -> bool {
    if options.is_empty() {
        return false;
    }
    let current = session
        .active
        .as_ref()
        .and_then(|active| options.iter().position(|option| &option.key == active));
    let next = match current {
        Some(index) => (index as isize + delta).rem_euclid(options.len() as isize) as usize,
        None if delta < 0 => options.len() - 1,
        None => 0,
    };
    session.active = Some(options[next].key.clone());
    session.engaged = true;
    true
}

pub fn tab_option<'a>(
    session: &AssistSession,
    options: &'a [CompletionOption],
) -> Option<&'a CompletionOption> {
    session
        .active
        .as_ref()
        .and_then(|active| options.iter().find(|option| &option.key == active))
        .or_else(|| options.first())
}

pub fn enter_option<'a>(
    session: &AssistSession,
    options: &'a [CompletionOption],
) -> Option<&'a CompletionOption> {
    if !session.engaged {
        return None;
    }
    let active = session.active.as_ref()?;
    options.iter().find(|option| &option.key == active)
}

struct FuzzyMatch {
    score: i32,
    ranges: Vec<Range<usize>>,
}

fn fuzzy_match(pattern: &str, candidate: &str) -> Option<FuzzyMatch> {
    let pattern = pattern
        .chars()
        .filter(|character| !character.is_control())
        .map(|character| character.to_ascii_lowercase())
        .collect::<Vec<_>>();
    if pattern.is_empty() {
        return Some(FuzzyMatch {
            score: 0,
            ranges: Vec::new(),
        });
    }

    let candidate_chars = candidate
        .char_indices()
        .map(|(offset, character)| {
            (
                offset,
                offset + character.len_utf8(),
                character.to_ascii_lowercase(),
            )
        })
        .collect::<Vec<_>>();
    if candidate_chars.len() < pattern.len() {
        return None;
    }

    let mut score = 0;
    let mut search_from = 0;
    let mut first_match = None;
    let mut previous_match = None;
    let mut ranges = Vec::with_capacity(pattern.len());
    for expected in pattern {
        let matched = candidate_chars
            .iter()
            .enumerate()
            .skip(search_from)
            .find_map(|(index, (_, _, character))| (*character == expected).then_some(index))?;
        first_match.get_or_insert(matched);
        score += 1_000;
        if matched == 0 {
            score += 250;
        } else if is_word_start(candidate_chars[matched - 1].2) {
            score += 180;
        }
        if let Some(previous) = previous_match {
            if matched == previous + 1 {
                score += 350;
            } else {
                score -= (matched - previous - 1) as i32 * 12;
            }
        }
        let (start, end, _) = candidate_chars[matched];
        ranges.push(start..end);
        previous_match = Some(matched);
        search_from = matched + 1;
    }
    score -= first_match.unwrap_or(0) as i32 * 24;
    score -= candidate_chars.len() as i32;
    Some(FuzzyMatch { score, ranges })
}

fn is_word_start(character: char) -> bool {
    matches!(character, ' ' | '-' | '_' | '/' | '\\' | ':' | '(' | '[')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(name: &str, arg: CommandArgKind) -> CommandInfo {
        CommandInfo {
            name: name.into(),
            usage: name.into(),
            description: "test".into(),
            arg,
            placeholder: (arg == CommandArgKind::Free).then(|| "value".into()),
        }
    }

    #[test]
    fn derives_command_and_argument_contexts() {
        let commands = vec![
            command("/mute", CommandArgKind::None),
            command("/room", CommandArgKind::Room),
        ];
        assert_eq!(
            completion_context("/mu", 3..3, true, false, &commands),
            Some(CompletionContext::Command {
                query: "/mu".into(),
                span: 0..3,
            })
        );
        assert!(matches!(
            completion_context("/room gen", 9..9, true, false, &commands),
            Some(CompletionContext::Argument {
                kind: ArgumentKind::Candidates(CommandCandidateKind::Room),
                query,
                span,
                ..
            }) if query == "gen" && span == (6..9)
        ));
    }

    #[test]
    fn ignores_escaped_selected_composing_and_normal_mode_inputs() {
        let commands = vec![command("/mute", CommandArgKind::None)];
        for context in [
            completion_context(" /mute", 6..6, true, false, &commands),
            completion_context("/mute", 1..3, true, false, &commands),
            completion_context("/mute", 5..5, true, true, &commands),
            completion_context("/mute", 5..5, false, false, &commands),
        ] {
            assert!(context.is_none());
        }
    }

    #[test]
    fn fuzzy_ranking_rewards_tight_matches_and_tracks_utf8_ranges() {
        let commands = vec![
            command("/audio-reset", CommandArgKind::None),
            command("/room", CommandArgKind::Room),
            command("/røøm", CommandArgKind::Room),
        ];
        let rows = command_options(&commands, "/rm");
        assert_eq!(option_label(&rows[0]), "/room");
        let unicode = command_options(&commands, "/rø");
        assert_eq!(option_label(&unicode[0]), "/røøm");
        assert!(
            unicode[0]
                .match_ranges
                .iter()
                .all(|range| "/røøm".is_char_boundary(range.start)
                    && "/røøm".is_char_boundary(range.end))
        );
    }

    #[test]
    fn navigation_is_passive_until_engaged_and_wraps() {
        let commands = vec![
            command("/mute", CommandArgKind::None),
            command("/room", CommandArgKind::Room),
        ];
        let context = completion_context("/", 1..1, true, false, &commands).unwrap();
        let options = command_options(&commands, "/");
        let mut session = open_session(&context);
        assert!(enter_option(&session, &options).is_none());
        assert_eq!(
            option_label(tab_option(&session, &options).unwrap()),
            "/mute"
        );
        assert!(move_selection(&mut session, &options, -1));
        assert_eq!(
            option_label(enter_option(&session, &options).unwrap()),
            "/room"
        );
        assert!(move_selection(&mut session, &options, 1));
        assert_eq!(
            option_label(enter_option(&session, &options).unwrap()),
            "/mute"
        );
    }

    #[test]
    fn command_acceptance_enters_argument_mode() {
        let commands = vec![command("/room", CommandArgKind::Room)];
        let context = completion_context("/ro", 3..3, true, false, &commands).unwrap();
        let option = command_options(&commands, "/ro").remove(0);
        assert_eq!(
            replacement(&context, &option),
            Replacement {
                span: 0..3,
                text: "/room ".into(),
            }
        );
    }
}
