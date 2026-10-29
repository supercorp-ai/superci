//! Changes to a control plane's settings (see `plane::SETTINGS`), as computers write them: each to one field, with what
//! the writer saw there when it showed one. The control plane applies them to the settings as they are now, so changes
//! made at the same moment to different fields are all kept, and one made over a field changed since it was shown is
//! refused for that field alone (`Conflict`).
//!
//! The settings are one JSON document (`{ "routing": …, "launchers": […] }`). A path names a field in it, one segment
//! a step: a key of an object, or, in a list kept by key (`keyed`: the order, the launchers), the key of an item in it.
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

/// One change. A field set to `null` is taken out (so is an item of a keyed list).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum Change {
    /// The field set to `value`. With `from`: only while it still holds that (`null`: while it is not there).
    Set {
        path: Vec<String>,
        value: Value,
        #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "present")]
        from: Option<Value>,
    },
    /// `value` added to a list (once: one already there is not added again), at its end.
    Add { path: Vec<String>, value: Value },
    /// `value` taken out of a list.
    Remove { path: Vec<String>, value: Value },
    /// A keyed list put in this order of its keys: an item not named (added meanwhile) after them, as it stood; a key
    /// naming none (taken out meanwhile) passed over.
    Arrange { path: Vec<String>, keys: Vec<String> },
}

impl Change {
    /// The field it changes.
    pub fn path(&self) -> &[String] { match self { Change::Set { path, .. } | Change::Add { path, .. } | Change::Remove { path, .. } | Change::Arrange { path, .. } => path } }
}

/// A field that no longer holds what the writer saw: what it holds now (`null`: nothing).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Conflict { pub path: Vec<String>, pub now: Value }

/// `from` as given, `null` too (`null` is "not there", not "no check").
fn present<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> { Value::deserialize(d).map(Some) }

/// The field its items are kept by, for a list kept by key: the order's providers and the launchers, by their cloud.
pub fn keyed(path: &[String]) -> Option<&'static str> {
    match path { [a, b] if a == "routing" && b == "order" => Some("cloud"), [a] if a == "launchers" => Some("cloud"), _ => None }
}

fn key_of<'a>(item: &'a Value, by: &str) -> Option<&'a str> { item[by].as_str() }

/// The field at `path`, if it is there (`null` is not).
pub fn get<'a>(doc: &'a Value, path: &[String]) -> Option<&'a Value> {
    let mut at = doc;
    for (i, step) in path.iter().enumerate() {
        at = match keyed(&path[..i]) {
            Some(by) => at.as_array()?.iter().find(|item| key_of(item, by) == Some(step))?,
            None => at.get(step)?,
        };
    }
    Some(at).filter(|v| !v.is_null())
}

/// The changes applied, all or none: none when any of them finds its field changed since it was shown (each such one
/// said), or a step that is not there (an item of a keyed list taken out meanwhile).
pub fn apply(doc: &Value, changes: &[Change]) -> Result<Value, Vec<Conflict>> {
    let mut out = doc.clone();
    let mut conflicts = vec![];
    for change in changes {
        let path = change.path();
        if let Change::Set { from: Some(from), .. } = change {
            let now = get(&out, path).cloned().unwrap_or(Value::Null);
            if now != *from { conflicts.push(Conflict { path: path.to_vec(), now }); continue }
        }
        if !one(&mut out, change) { conflicts.push(Conflict { path: path.to_vec(), now: Value::Null }) }
    }
    if conflicts.is_empty() { Ok(out) } else { Err(conflicts) }
}

/// The list or object a change's last step is in, made where an object on the way is not there yet; none where an
/// item of a keyed list on the way is not there, or the way is not through objects and lists.
fn parent<'a>(doc: &'a mut Value, path: &[String]) -> Option<&'a mut Value> {
    let mut at = doc;
    for (i, step) in path.iter().enumerate() {
        if at.is_null() { *at = Value::Object(Default::default()) }
        at = match keyed(&path[..i]) {
            Some(by) => at.as_array_mut()?.iter_mut().find(|item| key_of(item, by) == Some(step))?,
            None => at.as_object_mut()?.entry(step.clone()).or_insert(Value::Null),
        };
    }
    Some(at)
}

/// One change applied: whether it could be (its way there).
fn one(doc: &mut Value, change: &Change) -> bool {
    match change {
        Change::Set { path, value, .. } => {
            let Some((last, way)) = path.split_last() else { *doc = value.clone(); return true };
            let Some(at) = parent(doc, way) else { return false };
            match keyed(way) {
                Some(by) => {
                    if at.is_null() { *at = Value::Array(vec![]) }
                    let Some(items) = at.as_array_mut() else { return false };
                    let i = items.iter().position(|item| key_of(item, by) == Some(last));
                    match (i, value.is_null()) {
                        (Some(i), true) => { items.remove(i); }
                        (Some(i), false) => items[i] = value.clone(),
                        (None, true) => {}
                        (None, false) => items.push(value.clone()),
                    }
                }
                None => {
                    if at.is_null() { *at = Value::Object(Default::default()) }
                    let Some(fields) = at.as_object_mut() else { return false };
                    if value.is_null() { fields.remove(last); } else { fields.insert(last.clone(), value.clone()); }
                }
            }
            true
        }
        Change::Add { path, value } | Change::Remove { path, value } => {
            let Some(at) = parent(doc, path) else { return false };
            if at.is_null() { *at = Value::Array(vec![]) }
            let Some(items) = at.as_array_mut() else { return false };
            match change {
                Change::Add { .. } => if !items.contains(value) { items.push(value.clone()) },
                _ => items.retain(|item| item != value),
            }
            true
        }
        Change::Arrange { path, keys } => {
            let (Some(by), Some(at)) = (keyed(path), parent(doc, path)) else { return false };
            let Some(items) = at.as_array_mut() else { return false };
            let mut rest = std::mem::take(items);
            for key in keys {
                if let Some(i) = rest.iter().position(|item| key_of(item, by) == Some(key)) { items.push(rest.remove(i)) }
            }
            items.extend(rest);
            true
        }
    }
}

/// The changes that make `after` of `before`, field by field: what a writer changed and nothing else. `seen`: each set
/// says what was there (`before`), for the control plane to refuse it where that changed meanwhile; without, each is
/// meant as said (a command's).
pub fn diff(before: &Value, after: &Value, seen: bool) -> Vec<Change> {
    let mut out = vec![];
    walk(&mut out, &mut vec![], before, after);
    if !seen { for c in &mut out { if let Change::Set { from, .. } = c { *from = None } } }
    out
}

fn walk(out: &mut Vec<Change>, path: &mut Vec<String>, before: &Value, after: &Value) {
    if before == after { return }
    let set = |out: &mut Vec<Change>, path: &[String]| out.push(Change::Set { path: path.to_vec(), value: after.clone(), from: Some(before.clone()) });
    match (before, after, keyed(path)) {
        (Value::Object(b), Value::Object(a), _) => {
            let keys: std::collections::BTreeSet<&String> = b.keys().chain(a.keys()).collect();
            for k in keys {
                path.push(k.clone());
                walk(out, path, b.get(k).unwrap_or(&Value::Null), a.get(k).unwrap_or(&Value::Null));
                path.pop();
            }
        }
        // (A keyed list not there yet is an empty one.)
        (Value::Array(_) | Value::Null, Value::Array(_) | Value::Null, Some(by)) => {
            let none = vec![];
            let (b, a) = (before.as_array().unwrap_or(&none), after.as_array().unwrap_or(&none));
            let find = |items: &[Value], key: &str| items.iter().find(|item| key_of(item, by) == Some(key)).cloned();
            let mut order: Vec<String> = vec![];
            for item in b {
                let Some(key) = key_of(item, by) else { continue };
                path.push(key.to_string());
                match find(a, key) { Some(now) => { walk(out, path, item, &now); order.push(key.to_string()) } None => out.push(Change::Set { path: path.clone(), value: Value::Null, from: Some(item.clone()) }) }
                path.pop();
            }
            for item in a {
                let Some(key) = key_of(item, by) else { continue };
                if find(b, key).is_some() { continue }
                path.push(key.to_string());
                out.push(Change::Set { path: path.clone(), value: item.clone(), from: Some(Value::Null) });
                path.pop();
                order.push(key.to_string());
            }
            let keys: Vec<String> = a.iter().filter_map(|item| key_of(item, by).map(str::to_string)).collect();
            if keys != order { out.push(Change::Arrange { path: path.clone(), keys }) }
        }
        // A list of values (one not there yet: an empty one): each taken out and added; where that does not give its
        // order, the list set whole.
        (Value::Array(_) | Value::Null, Value::Array(_) | Value::Null, None) => {
            let none = vec![];
            let (b, a) = (before.as_array().unwrap_or(&none), after.as_array().unwrap_or(&none));
            let mut made: Vec<&Value> = b.iter().filter(|v| a.contains(v)).collect();
            made.extend(a.iter().filter(|v| !b.contains(v)));
            if made != a.iter().collect::<Vec<_>>() { return set(out, path) }
            for v in b.iter().filter(|v| !a.contains(v)) { out.push(Change::Remove { path: path.clone(), value: v.clone() }) }
            for v in a.iter().filter(|v| !b.contains(v)) { out.push(Change::Add { path: path.clone(), value: v.clone() }) }
        }
        _ => set(out, path),
    }
}
