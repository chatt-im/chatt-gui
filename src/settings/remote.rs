use std::collections::BTreeMap;

use local_rpc::settings as wire;

pub(super) type RemoteField = wire::SettingsFieldId;
pub(super) type RemoteSection = usize;

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct RemoteValues(BTreeMap<RemoteField, wire::SettingsValue>);

impl RemoteValues {
    pub(super) fn current(document: &wire::SettingsDocument) -> Self {
        Self(
            document
                .sections
                .iter()
                .flat_map(|section| &section.fields)
                .map(|field| (field.id, field.value.clone()))
                .collect(),
        )
    }

    pub(super) fn defaults(document: &wire::SettingsDocument) -> Self {
        Self(
            document
                .sections
                .iter()
                .flat_map(|section| &section.fields)
                .map(|field| (field.id, field.default.clone()))
                .collect(),
        )
    }

    pub(super) fn get(&self, field: RemoteField) -> Option<&wire::SettingsValue> {
        self.0.get(&field)
    }

    pub(super) fn set(&mut self, field: RemoteField, value: wire::SettingsValue) {
        self.0.insert(field, value);
    }

    pub(super) fn copy_from(&mut self, source: &Self, field: RemoteField) {
        if let Some(value) = source.get(field) {
            self.set(field, value.clone());
        }
    }

    pub(super) fn changes(
        &self,
        baseline: &Self,
        include: impl Fn(RemoteField) -> bool,
    ) -> Vec<wire::SettingsChange> {
        self.0
            .iter()
            .filter(|(field, value)| include(**field) && baseline.get(**field) != Some(*value))
            .map(|(field, value)| wire::SettingsChange {
                field: *field,
                value: value.clone(),
            })
            .collect()
    }
}

pub(super) fn field(
    sections: &[wire::SettingsSection],
    id: RemoteField,
) -> Option<&wire::SettingsField> {
    sections
        .iter()
        .flat_map(|section| &section.fields)
        .find(|field| field.id == id)
}

pub(super) fn fields(
    sections: &[wire::SettingsSection],
    section: RemoteSection,
    advanced: bool,
) -> Vec<RemoteField> {
    sections
        .get(section)
        .into_iter()
        .flat_map(|section| &section.fields)
        .filter(|field| supported(field))
        .filter(|field| advanced || field.flags & wire::FIELD_ADVANCED == 0)
        .map(|field| field.id)
        .collect()
}

pub(super) fn matches_search(
    sections: &[wire::SettingsSection],
    section: RemoteSection,
    id: RemoteField,
    query: &str,
) -> bool {
    let query = query.trim().to_ascii_lowercase();
    if query.is_empty() {
        return true;
    }
    let Some(section) = sections.get(section) else {
        return false;
    };
    let Some(field) = section.fields.iter().find(|field| field.id == id) else {
        return false;
    };
    [
        section.title.as_str(),
        section.help.as_str(),
        field.key.as_str(),
        field.label.as_str(),
        field.help.as_str(),
    ]
    .into_iter()
    .any(|value| value.to_ascii_lowercase().contains(&query))
}

pub(super) fn supported(field: &wire::SettingsField) -> bool {
    matches!(
        field.control.kind,
        wire::CONTROL_TOGGLE
            | wire::CONTROL_NUMBER
            | wire::CONTROL_TEXT
            | wire::CONTROL_TEXT_LIST
            | wire::CONTROL_CHOICE
            | wire::CONTROL_SEARCHABLE_CHOICE
    )
}

pub(super) fn is_text(field: &wire::SettingsField) -> bool {
    matches!(
        field.control.kind,
        wire::CONTROL_NUMBER | wire::CONTROL_TEXT | wire::CONTROL_TEXT_LIST
    )
}

pub(super) fn is_audio(field: &wire::SettingsField) -> bool {
    field.flags & wire::FIELD_AUDIO != 0
}

pub(super) fn value(values: &RemoteValues, field: &wire::SettingsField) -> String {
    let Some(value) = values.get(field.id) else {
        return String::new();
    };
    match value {
        wire::SettingsValue::Bool(value) => if *value { "On" } else { "Off" }.into(),
        wire::SettingsValue::Signed(value) => number_with_unit(value, &field.control.unit),
        wire::SettingsValue::Unsigned(value) => number_with_unit(value, &field.control.unit),
        wire::SettingsValue::Float(value) => number_with_unit(value, &field.control.unit),
        wire::SettingsValue::Text(value)
            if matches!(
                field.control.kind,
                wire::CONTROL_CHOICE | wire::CONTROL_SEARCHABLE_CHOICE
            ) =>
        {
            field
                .control
                .choices
                .iter()
                .find(|choice| choice.value == *value)
                .map_or_else(|| value.clone(), |choice| choice.label.clone())
        }
        wire::SettingsValue::Text(value) => value.clone(),
        wire::SettingsValue::TextList(values) => values.join("\n"),
    }
}

fn number_with_unit(value: impl std::fmt::Display, unit: &str) -> String {
    if unit.is_empty() {
        value.to_string()
    } else {
        format!("{value} {unit}")
    }
}

pub(super) fn editor_value(values: &RemoteValues, field: &wire::SettingsField) -> String {
    match values.get(field.id) {
        Some(wire::SettingsValue::Signed(value)) => value.to_string(),
        Some(wire::SettingsValue::Unsigned(value)) => value.to_string(),
        Some(wire::SettingsValue::Float(value)) => value.to_string(),
        Some(wire::SettingsValue::Text(value)) => value.clone(),
        Some(wire::SettingsValue::TextList(values)) => values.join("\n"),
        Some(wire::SettingsValue::Bool(value)) => value.to_string(),
        None => String::new(),
    }
}

pub(super) fn apply_text(
    values: &mut RemoteValues,
    field: &wire::SettingsField,
    text: &str,
) -> Result<(), String> {
    let value = match values.get(field.id).unwrap_or(&field.value) {
        wire::SettingsValue::Signed(_) => wire::SettingsValue::Signed(parse_number(text, field)?),
        wire::SettingsValue::Unsigned(_) => {
            wire::SettingsValue::Unsigned(parse_number(text, field)?)
        }
        wire::SettingsValue::Float(_) => wire::SettingsValue::Float(parse_number(text, field)?),
        wire::SettingsValue::Text(_) => wire::SettingsValue::Text(text.to_owned()),
        wire::SettingsValue::TextList(_) => wire::SettingsValue::TextList(
            text.lines()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
                .collect(),
        ),
        wire::SettingsValue::Bool(_) => return Err("this setting is not text-editable".into()),
    };
    values.set(field.id, value);
    Ok(())
}

fn parse_number<T>(text: &str, field: &wire::SettingsField) -> Result<T, String>
where
    T: std::str::FromStr + IntoNumber,
{
    let value: T = text
        .trim()
        .parse()
        .map_err(|_| "enter a valid number".to_string())?;
    let number = value.as_f64();
    if field
        .control
        .min
        .is_some_and(|minimum| number < f64::from(minimum))
        || field
            .control
            .max
            .is_some_and(|maximum| number > f64::from(maximum))
    {
        return Err(match (field.control.min, field.control.max) {
            (Some(minimum), Some(maximum)) => {
                format!("value must be between {minimum} and {maximum}")
            }
            (Some(minimum), None) => format!("value must be at least {minimum}"),
            (None, Some(maximum)) => format!("value must be at most {maximum}"),
            (None, None) => unreachable!(),
        });
    }
    Ok(value)
}

trait IntoNumber {
    fn as_f64(&self) -> f64;
}

impl IntoNumber for i64 {
    fn as_f64(&self) -> f64 {
        *self as f64
    }
}

impl IntoNumber for u64 {
    fn as_f64(&self) -> f64 {
        *self as f64
    }
}

impl IntoNumber for f32 {
    fn as_f64(&self) -> f64 {
        f64::from(*self)
    }
}

pub(super) fn cycle(values: &mut RemoteValues, field: &wire::SettingsField, delta: isize) -> bool {
    match values.get(field.id) {
        Some(wire::SettingsValue::Bool(value)) if field.control.kind == wire::CONTROL_TOGGLE => {
            values.set(field.id, wire::SettingsValue::Bool(!*value));
            true
        }
        Some(wire::SettingsValue::Text(value))
            if field.control.kind == wire::CONTROL_CHOICE && !field.control.choices.is_empty() =>
        {
            let current = field
                .control
                .choices
                .iter()
                .position(|choice| choice.value == *value)
                .unwrap_or(0);
            let next = (current as isize + delta).rem_euclid(field.control.choices.len() as isize)
                as usize;
            let value = field.control.choices[next].value.clone();
            values.set(field.id, wire::SettingsValue::Text(value));
            true
        }
        _ => false,
    }
}
