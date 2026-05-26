//! Compare predicted Panel against ground-truth and roll up accuracy.

use crate::{Item, Panel};

#[derive(Debug, Default, Clone)]
pub struct Row {
    pub title_ok: bool,
    pub level_ok: bool,
    pub cost_ok: bool,
    pub item_count_ok: bool,
    pub items_name_correct: u32,
    pub items_needed_correct: u32,
    pub items_collected_correct: u32,
    pub items_total: u32,
    pub expected_summary: String,
    pub predicted_summary: String,
}

impl Row {
    pub fn render(&self, image: &str) -> String {
        let dot = |b: bool| if b { "v" } else { "x" };
        format!(
            "{image:38} title:{} level:{} cost:{} items:{}  names:{}/{} need:{}/{} got:{}/{}\n   expected: {}\n   got:      {}",
            dot(self.title_ok),
            dot(self.level_ok),
            dot(self.cost_ok),
            dot(self.item_count_ok),
            self.items_name_correct,
            self.items_total,
            self.items_needed_correct,
            self.items_total,
            self.items_collected_correct,
            self.items_total,
            self.expected_summary,
            self.predicted_summary,
        )
    }
}

pub fn compare(expected: &Panel, predicted: &Panel) -> Row {
    let title_ok = normalize_title(&expected.title) == normalize_title(&predicted.title);
    let level_ok = expected.level.eq_ignore_ascii_case(&predicted.level);
    let cost_ok = expected.cost == predicted.cost;
    let item_count_ok = expected.items.len() == predicted.items.len();

    let mut name_ok = 0u32;
    let mut need_ok = 0u32;
    let mut coll_ok = 0u32;
    let pairs = expected.items.iter().zip(predicted.items.iter());
    let total = expected.items.len() as u32;
    for (e, p) in pairs {
        if normalize_name(&e.name) == normalize_name(&p.name) {
            name_ok += 1;
        }
        if e.needed == p.needed {
            need_ok += 1;
        }
        if e.collected == p.collected {
            coll_ok += 1;
        }
    }

    Row {
        title_ok,
        level_ok,
        cost_ok,
        item_count_ok,
        items_name_correct: name_ok,
        items_needed_correct: need_ok,
        items_collected_correct: coll_ok,
        items_total: total,
        expected_summary: summarize(expected),
        predicted_summary: summarize(predicted),
    }
}

fn summarize(panel: &Panel) -> String {
    let items = panel
        .items
        .iter()
        .map(|i| format!("{}={}/{}", i.name, i.collected, i.needed))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "{} {} cost={} [{items}]",
        panel.title,
        panel.level,
        panel.cost.map(|c| c.to_string()).unwrap_or_else(|| "?".into())
    )
}

fn normalize_title(s: &str) -> String {
    s.trim().to_ascii_lowercase()
}

fn normalize_name(s: &str) -> String {
    s.trim().to_ascii_lowercase()
}

#[allow(dead_code)]
pub fn nonzero_items_count(items: &[Item]) -> usize {
    items.iter().filter(|i| i.needed > 0).count()
}

#[derive(Debug, Default)]
pub struct Report {
    rows: u32,
    title_ok: u32,
    level_ok: u32,
    cost_ok: u32,
    count_ok: u32,
    name_ok: u32,
    need_ok: u32,
    coll_ok: u32,
    item_total: u32,
}

impl Report {
    pub fn add(&mut self, r: &Row) {
        self.rows += 1;
        if r.title_ok {
            self.title_ok += 1;
        }
        if r.level_ok {
            self.level_ok += 1;
        }
        if r.cost_ok {
            self.cost_ok += 1;
        }
        if r.item_count_ok {
            self.count_ok += 1;
        }
        self.name_ok += r.items_name_correct;
        self.need_ok += r.items_needed_correct;
        self.coll_ok += r.items_collected_correct;
        self.item_total += r.items_total;
    }

    pub fn render(&self) -> String {
        let pct = |n: u32, d: u32| {
            if d == 0 {
                "n/a".to_string()
            } else {
                format!("{}/{} ({:.0}%)", n, d, 100.0 * n as f32 / d as f32)
            }
        };
        format!(
            "title:        {}\nlevel:        {}\ncost:         {}\nitem-count:   {}\nitem names:   {}\nitem needed:  {}\nitem collect: {}",
            pct(self.title_ok, self.rows),
            pct(self.level_ok, self.rows),
            pct(self.cost_ok, self.rows),
            pct(self.count_ok, self.rows),
            pct(self.name_ok, self.item_total),
            pct(self.need_ok, self.item_total),
            pct(self.coll_ok, self.item_total),
        )
    }
}
