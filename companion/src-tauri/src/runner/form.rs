//! MCP elicitation forms, shared by both runners: the one place that decides whether a form's
//! schema is one a Slack answer can fill, and turns an answer into the content sent back.
//!
//! The supported subset: a root `{"type": "object", "properties": {...}}` (with optional
//! `required`, `title`, `description`, `$schema`), and fields of type `string` (free text, or
//! one of an `enum` or `oneOf` list of strings), `number`, `integer` or `boolean`, each with
//! optional `title`, `description` and a `default` of its own type. Anything else (another root,
//! a constraint such as `minLength`, `format`, `minimum` or a boolean `enum`, a contradiction
//! such as a `required` name that is no field) is unsupported: the form can only be declined, so
//! an answer never claims to meet a rule nobody checked.

use serde_json::{Map, Value};

/// Keo's cap on an option's length; a longer value shows shortened.
pub const LABEL_MAX: usize = 75;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Text,
    Number,
    Integer,
    Boolean,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub name: String,
    pub title: Option<String>,
    pub kind: Kind,
    /// The whole list a text field takes its value from; empty for free text.
    pub values: Vec<String>,
    pub required: bool,
}

/// The button label for `value`: the value, shortened to `LABEL_MAX` UTF-16 units (as the
/// bridge counts) with a marked cut.
pub fn label(value: &str) -> String {
    if super::units(value) <= LABEL_MAX {
        return value.to_string();
    }
    let mut out = super::prefix_units(value, LABEL_MAX - 1).to_string();
    out.push('…');
    out
}

/// Button labels the runners answer with themselves; a listed value may not read as one.
const CONTROLS: &[&str] = &["Decline", "Deny", "Allow", "Accept", "Done"];

fn only(o: &Map<String, Value>, keys: &[&str]) -> bool {
    o.keys().all(|k| keys.contains(&k.as_str()))
}

fn optional_string(o: &Map<String, Value>, key: &str) -> Result<Option<String>, String> {
    match o.get(key) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(format!("`{key}` is not text")),
    }
}

/// The fields of a supported form schema, in schema order (none: a plain confirmation), or why
/// it is not one.
pub fn parse(schema: &Value) -> Result<Vec<Field>, String> {
    let root = schema.as_object().ok_or("the form has no schema")?;
    if !only(
        root,
        &[
            "$schema",
            "type",
            "properties",
            "required",
            "title",
            "description",
        ],
    ) {
        return Err("the form has a rule this cannot check".into());
    }
    if root.get("type").and_then(Value::as_str) != Some("object") {
        return Err("the form is not an object form".into());
    }
    optional_string(root, "title")?;
    optional_string(root, "description")?;
    let props = root
        .get("properties")
        .and_then(Value::as_object)
        .ok_or("the form lists no fields")?;
    let required: Vec<&str> = match root.get("required") {
        None => Vec::new(),
        Some(Value::Array(a)) => a
            .iter()
            .map(|r| r.as_str().ok_or("a required name is not text"))
            .collect::<Result<_, _>>()?,
        Some(_) => return Err("`required` is not a list".into()),
    };
    for (i, r) in required.iter().enumerate() {
        if !props.contains_key(*r) || required[..i].contains(r) {
            return Err(format!("`required` names {r:?} badly"));
        }
    }
    props
        .iter()
        .map(|(name, schema)| field(name, schema, required.contains(&name.as_str())))
        .collect()
}

fn field(name: &str, schema: &Value, required: bool) -> Result<Field, String> {
    let bad = |why: &str| Err(format!("field {name:?} {why}"));
    let Some(f) = schema.as_object() else {
        return bad("is not a field schema");
    };
    let kind = match f.get("type").and_then(Value::as_str) {
        Some("string") => Kind::Text,
        Some("number") => Kind::Number,
        Some("integer") => Kind::Integer,
        Some("boolean") => Kind::Boolean,
        _ => return bad("is of a type Slack cannot fill"),
    };
    let allowed: &[&str] = match kind {
        Kind::Text => &["type", "title", "description", "default", "enum", "oneOf"],
        _ => &["type", "title", "description", "default"],
    };
    if !only(f, allowed) {
        return bad("has a rule this cannot check");
    }
    let title = optional_string(f, "title")?;
    optional_string(f, "description")?;
    let values: Vec<String> = match (f.get("enum"), f.get("oneOf")) {
        (Some(_), Some(_)) => return bad("has both `enum` and `oneOf`"),
        (Some(Value::Array(e)), None) => {
            let mut out = Vec::new();
            for v in e {
                match v.as_str() {
                    Some(v) => out.push(v.to_string()),
                    None => return bad("has a value that is not text"),
                }
            }
            out
        }
        (None, Some(Value::Array(o))) => {
            let mut out = Vec::new();
            for c in o {
                let c = c.as_object().filter(|c| only(c, &["const", "title"]));
                let v = c.and_then(|c| c.get("const")).and_then(Value::as_str);
                let titled =
                    c.is_some_and(|c| matches!(c.get("title"), None | Some(Value::String(_))));
                match v {
                    Some(v) if titled => out.push(v.to_string()),
                    _ => return bad("has a choice this cannot read"),
                }
            }
            out
        }
        (None, None) => Vec::new(),
        _ => return bad("has a list that is not a list"),
    };
    let listed = f.contains_key("enum") || f.contains_key("oneOf");
    if listed && values.is_empty() {
        return bad("lists no value");
    }
    // Each value must survive the trip to a button and back unchanged and unmistaken: a value
    // with outer spaces (answers are trimmed), two values whose labels read the same once clipped
    // and case-folded, or a label that reads as a control button, makes the form Decline-only.
    if values.iter().any(String::is_empty) {
        return bad("lists an empty value");
    }
    if values.iter().any(|v| v.trim() != v) {
        return bad("has a value with outer spaces");
    }
    let shown: Vec<String> = values.iter().map(|v| label(v).to_lowercase()).collect();
    if (0..shown.len()).any(|i| shown[..i].contains(&shown[i])) {
        return bad("lists two values that read the same");
    }
    if shown
        .iter()
        .any(|l| CONTROLS.iter().any(|c| c.eq_ignore_ascii_case(l)))
    {
        return bad("lists a value that reads as a control button");
    }
    let field = Field {
        name: name.to_string(),
        title,
        kind,
        values,
        required,
    };
    if let Some(d) = f.get("default") {
        if value_of(&field, d).is_none() {
            return bad("has a default it does not allow");
        }
    }
    Ok(field)
}

/// `v` when it is a value `field` takes as it is (a `default`).
pub fn value_of(field: &Field, v: &Value) -> Option<Value> {
    let ok = match (field.kind, v) {
        (Kind::Text, Value::String(s)) => field.values.is_empty() || field.values.contains(s),
        (Kind::Boolean, Value::Bool(_)) => true,
        (Kind::Integer, Value::Number(n)) => n.is_i64() || n.is_u64(),
        (Kind::Number, Value::Number(n)) => n.as_f64().is_some_and(f64::is_finite),
        _ => false,
    };
    ok.then(|| v.clone())
}

/// The value `words` (a person's answer, or a button label) give `field`, or `None` when they
/// are not one it takes: a listed value, or the one value a shortened label stands for; a
/// whole number; a finite number; yes or no.
pub fn coerce(field: &Field, words: &str) -> Option<Value> {
    let w = words.trim();
    match field.kind {
        Kind::Boolean => match w.to_ascii_lowercase().as_str() {
            "yes" | "true" | "y" => Some(Value::Bool(true)),
            "no" | "false" | "n" => Some(Value::Bool(false)),
            _ => None,
        },
        Kind::Integer => w.parse::<i64>().ok().map(Value::from),
        Kind::Number => w
            .parse::<f64>()
            .ok()
            .filter(|n| n.is_finite())
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number),
        Kind::Text if field.values.is_empty() => {
            (!w.is_empty()).then(|| Value::String(w.to_string()))
        }
        Kind::Text => {
            if field.values.iter().any(|v| v == w) {
                return Some(Value::String(w.to_string()));
            }
            let labelled: Vec<&String> = field.values.iter().filter(|v| label(v) == w).collect();
            match labelled.as_slice() {
                [only] => Some(Value::String((*only).clone())),
                _ => None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn one(props: Value) -> Result<Vec<Field>, String> {
        parse(&json!({"type": "object", "properties": {"x": props}}))
    }

    #[test]
    fn only_the_supported_subset_parses() {
        assert_eq!(
            parse(&json!({"type": "object", "properties": {}})),
            Ok(vec![])
        );
        let ok = one(json!({"type": "string", "enum": ["a", "b"], "default": "a", "title": "X"}))
            .unwrap();
        assert_eq!(ok[0].values, vec!["a", "b"]);
        assert!(one(json!({"type": "boolean", "default": true})).is_ok());
        assert!(
            one(json!({"type": "string", "oneOf": [{"const": "a", "title": "A"}]})).unwrap()[0]
                .values
                == vec!["a"]
        );
        for bad in [
            Value::Null,
            json!({"properties": {}}),
            json!({"type": "string", "properties": {}}),
            json!({"type": "object"}),
            json!({"type": "object", "properties": {}, "additionalProperties": true}),
            json!({"type": "object", "properties": {"x": {"type": "string"}}, "required": ["y"]}),
            json!({"type": "object", "properties": {"x": {"type": "string"}}, "required": "x"}),
        ] {
            assert!(parse(&bad).is_err(), "{bad}");
        }
        for bad in [
            json!({}),
            json!({"type": "array"}),
            json!({"type": "boolean", "enum": [true]}),
            json!({"type": "string", "minLength": 3}),
            json!({"type": "string", "format": "email"}),
            json!({"type": "number", "minimum": 1}),
            json!({"type": "string", "enum": []}),
            json!({"type": "string", "enum": [1]}),
            json!({"type": "string", "enum": ["a", "a"]}),
            json!({"type": "string", "enum": ["Decline", "x"]}),
            json!({"type": "string", "enum": ["x", "deny"]}),
            json!({"type": "string", "enum": ["x", " x "]}),
            json!({"type": "string", "enum": ["x", "X"]}),
            json!({"type": "string", "enum": [" x"]}),
            json!({"type": "string", "enum": ["", "x"]}),
            json!({"type": "string", "oneOf": [{"const": ""}]}),
            json!({"type": "string", "oneOf": [{"const": "allow"}]}),
            json!({"type": "string", "enum": ["y".repeat(LABEL_MAX + 1), "y".repeat(LABEL_MAX + 2)]}),
            json!({"type": "string", "enum": ["y".repeat(LABEL_MAX + 1), format!("{}…", "y".repeat(LABEL_MAX - 1))]}),
            json!({"type": "string", "enum": ["😀".repeat(LABEL_MAX / 2 + 1), "😀".repeat(LABEL_MAX / 2 + 2)]}),
            json!({"type": "string", "enum": ["a"], "oneOf": [{"const": "a"}]}),
            json!({"type": "string", "oneOf": [{"const": 1}]}),
            json!({"type": "string", "enum": ["a"], "default": "b"}),
            json!({"type": "integer", "default": 1.5}),
            json!({"type": "string", "title": 7}),
        ] {
            assert!(one(bad.clone()).is_err(), "{bad}");
        }
    }

    #[test]
    fn answers_take_only_what_the_field_allows() {
        let f = |props: Value| one(props).unwrap().remove(0);
        let yn = f(json!({"type": "boolean"}));
        assert_eq!(coerce(&yn, "No"), Some(json!(false)));
        assert_eq!(coerce(&yn, "yes please"), None);
        let n = f(json!({"type": "number"}));
        assert_eq!(coerce(&n, "2.5"), Some(json!(2.5)));
        assert_eq!(coerce(&n, "NaN"), None);
        assert_eq!(coerce(&n, "inf"), None);
        assert_eq!(coerce(&f(json!({"type": "integer"})), "2.5"), None);
        assert_eq!(coerce(&f(json!({"type": "string"})), "  "), None);
        let long = "x".repeat(LABEL_MAX + 5);
        let e = f(json!({"type": "string", "enum": [long.clone(), "b"]}));
        assert_eq!(coerce(&e, &label(&long)), Some(json!(long)));
        assert_eq!(coerce(&e, "c"), None);
        // Astral characters count two units each, so the label still fits the bridge's cut.
        let astral = "😀".repeat(LABEL_MAX / 2 + 3);
        let e = f(json!({"type": "string", "enum": [astral.clone(), "b"]}));
        assert!(super::super::units(&label(&astral)) <= LABEL_MAX);
        assert_eq!(coerce(&e, &label(&astral)), Some(json!(astral)));
    }
}
