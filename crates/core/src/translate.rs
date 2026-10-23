//! Label translation to the canonical model (1.2, 1.3).
//!
//! A repo's translation merges over the defaults value by value; labels match
//! case-insensitively; a label that translates to nothing is listed, never dropped.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::config::{ConfigSet, Labels, Translate};
use crate::docs::Priority;

/// One repo's translation, resolved: canonical value → the repo's label names, first preferred.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Translation {
    pub types: BTreeMap<String, Labels>,
    pub priorities: BTreeMap<Priority, Labels>,
    pub modifiers: BTreeMap<String, Labels>,
    pub areas: BTreeMap<String, Labels>,
}

/// What an item's labels mean in the canonical model.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Canonical {
    /// Exactly one type, else `None` (untriaged).
    pub type_: Option<String>,
    pub areas: Vec<String>,
    pub modifiers: Vec<String>,
    /// Exactly one priority label, else `None`.
    pub priority: Option<Priority>,
    pub untranslated: Vec<String>,
}

fn merge(into: &mut BTreeMap<String, Labels>, values: impl IntoIterator<Item = (&'static str, Option<Labels>)>) {
    for (k, v) in values {
        if let Some(v) = v {
            into.insert(k.to_string(), v);
        }
    }
}

fn apply(t: &mut Translation, layer: &Translate) {
    if let Some(ty) = &layer.type_ {
        merge(
            &mut t.types,
            [
                ("bug", ty.bug.clone()),
                ("todo", ty.todo.clone()),
                ("decision", ty.decision.clone()),
                ("docs", ty.docs.clone()),
            ],
        );
    }
    if let Some(p) = &layer.priority {
        for (k, v) in [
            (Priority::P0, &p.p0),
            (Priority::P1, &p.p1),
            (Priority::P2, &p.p2),
            (Priority::P3, &p.p3),
        ] {
            if let Some(v) = v {
                t.priorities.insert(k, v.clone());
            }
        }
    }
    if let Some(m) = &layer.modifier {
        merge(
            &mut t.modifiers,
            [
                ("needs_decision", m.needs_decision.clone()),
                ("in_progress", m.in_progress.clone()),
                ("self_contained", m.self_contained.clone()),
                ("partial", m.partial.clone()),
                ("umbrella", m.umbrella.clone()),
                ("good_first_issue", m.good_first_issue.clone()),
                ("help_wanted", m.help_wanted.clone()),
            ],
        );
    }
    if let Some(a) = &layer.area {
        for (k, v) in a {
            t.areas.insert(k.clone(), v.clone());
        }
    }
}

impl Translation {
    /// The translation in force for `repo`: the registry's defaults, then the repo file's
    /// areas (each name its own label), then the registry entry, value by value.
    pub fn for_repo(config: &ConfigSet, repo: &str) -> Translation {
        let mut t = Translation::default();
        if let Some(d) = config.registry.defaults.as_ref().and_then(|d| d.translate.as_ref()) {
            apply(&mut t, d);
        }
        if let Some(areas) = config.repo_files.get(repo).and_then(|f| f.areas.as_ref()) {
            for a in areas {
                t.areas.insert(a.clone(), vec![a.clone()]);
            }
        }
        if let Some(r) = config.registry.repo.get(repo).and_then(|e| e.translate.as_ref()) {
            apply(&mut t, r);
        }
        t
    }

    /// Whether the repo declares areas, so an item without one is untriaged (1.2).
    pub fn declares_areas(&self) -> bool {
        !self.areas.is_empty()
    }

    pub fn translate(&self, labels: &[String]) -> Canonical {
        let hit = |names: &Labels, label: &str| names.iter().any(|n| n.eq_ignore_ascii_case(label));
        let mut types = Vec::new();
        let mut priorities = Vec::new();
        let mut c = Canonical::default();
        for label in labels {
            let mut known = false;
            for (v, names) in &self.types {
                if hit(names, label) {
                    known = true;
                    if !types.contains(v) {
                        types.push(v.clone());
                    }
                }
            }
            for (v, names) in &self.priorities {
                if hit(names, label) {
                    known = true;
                    if !priorities.contains(v) {
                        priorities.push(*v);
                    }
                }
            }
            for (v, names) in &self.modifiers {
                if hit(names, label) {
                    known = true;
                    if !c.modifiers.contains(v) {
                        c.modifiers.push(v.clone());
                    }
                }
            }
            for (v, names) in &self.areas {
                if hit(names, label) {
                    known = true;
                    if !c.areas.contains(v) {
                        c.areas.push(v.clone());
                    }
                }
            }
            if !known {
                c.untranslated.push(label.clone());
            }
        }
        if let [one] = types.as_slice() {
            c.type_ = Some(one.clone());
        }
        if let [one] = priorities.as_slice() {
            c.priority = Some(*one);
        }
        c.areas.sort();
        c.modifiers.sort();
        c.untranslated.sort();
        c
    }
}
