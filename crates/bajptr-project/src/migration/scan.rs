use crate::{Control, Result, invalid, model::Edit};
use bajptr_core::{
    Error,
    database::{self, LocatedText, RowId},
};
use std::collections::{BTreeMap, HashMap, HashSet};

pub(crate) const MAX_BYTES: usize = 64 * 1024 * 1024;
const FUZZY_LIMIT: usize = 5;

#[derive(Default)]
pub(crate) struct Candidates {
    pub tables: Option<HashSet<String>>,
    pub locations: Vec<LocatedText>,
    pub fuzzy: HashMap<(String, String), Vec<LocatedText>>,
    pub skipped_tables: HashSet<String>,
}

fn grams(text: &str) -> HashSet<(char, char)> {
    let chars = text.chars();
    chars.clone().zip(chars.skip(1)).take(4096).collect()
}

pub(crate) fn collect(
    con: &rusqlite::Connection,
    edits: &[Edit],
    control: &Control<'_>,
) -> Result<Candidates> {
    let tables: HashSet<_> = database::tables(con, ".*")?.into_iter().collect();
    let mut wanted = BTreeMap::<&str, Vec<&Edit>>::new();
    for edit in edits {
        if tables.contains(&edit.table) {
            wanted.entry(&edit.table).or_default().push(edit);
        }
    }
    let mut result = Candidates::default();
    let mut bytes = 0usize;
    for (table, edits) in wanted {
        control.log(&format!("核对旧版修改：{}", crate::model::title(table)))?;
        let mut originals: Vec<_> = edits.iter().map(|e| e.original.as_str()).collect();
        originals.sort_unstable();
        originals.dedup();
        let exact: HashSet<_> = originals.iter().copied().collect();
        let addresses: HashSet<(&RowId, usize)> =
            edits.iter().map(|e| (&e.row_id, e.slot)).collect();
        let mut before = HashMap::<_, Vec<_>>::new();
        let mut after = HashMap::<_, Vec<_>>::new();
        for anchor in edits
            .iter()
            .filter_map(|e| e.anchor.as_ref())
            .collect::<HashSet<_>>()
        {
            if let Some(value) = anchor.previous.as_deref() {
                before.entry(value).or_default().push(anchor);
            }
            if let Some(value) = anchor.next.as_deref() {
                after.entry(value).or_default().push(anchor);
            }
        }
        let mut postings = HashMap::<_, Vec<usize>>::new();
        let sizes: Vec<_> = originals
            .iter()
            .enumerate()
            .map(|(i, text)| {
                let tokens = grams(text);
                for token in &tokens {
                    postings.entry(*token).or_default().push(i);
                }
                tokens.len()
            })
            .collect();
        let mut scores = HashMap::<usize, usize>::new();
        let mut best = vec![Vec::<(usize, LocatedText)>::new(); originals.len()];
        let scan = database::visit_locations(con, table, control, |row, prev, next| {
            let nearby = prev
                .and_then(|key| before.get(key))
                .into_iter()
                .flatten()
                .chain(next.and_then(|key| after.get(key)).into_iter().flatten())
                .copied()
                .collect::<HashSet<_>>();
            for (slot, text) in &row.fields {
                let exact_or_address =
                    exact.contains(text.as_str()) || addresses.contains(&(&row.id, *slot));
                let mut location = (exact_or_address || !nearby.is_empty())
                    .then(|| row.locate(table, *slot, text, prev, next));
                if exact_or_address
                    || location
                        .as_ref()
                        .and_then(|v| v.anchor.as_ref())
                        .is_some_and(|current| {
                            nearby.iter().any(|anchor| {
                                [
                                    (&anchor.previous, &current.previous),
                                    (&anchor.next, &current.next),
                                    (&anchor.siblings, &current.siblings),
                                ]
                                .into_iter()
                                .filter(|(old, new)| old.is_some() && old == new)
                                .count()
                                    >= 2
                            })
                        })
                {
                    let found = location.as_ref().unwrap();
                    bytes += cost(found);
                    if bytes > MAX_BYTES || result.locations.len() == 50_000 {
                        return Err(invalid("迁移候选过多，请分批核对旧版修改。"));
                    }
                    result.locations.push(found.clone());
                }
                if postings.is_empty() || text.chars().count() < 4 {
                    continue;
                }
                let tokens = grams(text);
                scores.clear();
                for ids in tokens.iter().filter_map(|token| postings.get(token)) {
                    for &i in ids {
                        *scores.entry(i).or_default() += 1;
                    }
                }
                for (&i, &overlap) in &scores {
                    if originals[i] == text || sizes[i] < 3 || overlap < 2 {
                        continue;
                    }
                    let score = 2000 * overlap / (sizes[i] + tokens.len());
                    let choices = &mut best[i];
                    if score < 500
                        || choices.len() == FUZZY_LIMIT
                            && choices.last().is_some_and(|(s, _)| *s >= score)
                    {
                        continue;
                    }
                    let found =
                        location.get_or_insert_with(|| row.locate(table, *slot, text, prev, next));
                    if choices.len() == FUZZY_LIMIT {
                        bytes -= cost(&choices.pop().unwrap().1);
                    }
                    bytes += cost(found);
                    if bytes > MAX_BYTES {
                        return Err(invalid("迁移候选过多，请分批核对旧版修改。"));
                    }
                    let at = choices.partition_point(|(s, _)| *s >= score);
                    choices.insert(at, (score, found.clone()));
                }
            }
            Ok(())
        });
        match scan {
            Err(Error::UnsupportedTable(_)) => {
                result.skipped_tables.insert(table.into());
            }
            other => other?,
        }
        for (original, choices) in originals.into_iter().zip(best) {
            if !choices.is_empty() {
                result.fuzzy.insert(
                    (table.into(), original.into()),
                    choices.into_iter().map(|(_, row)| row).collect(),
                );
            }
        }
    }
    result.tables = Some(tables);
    Ok(result)
}

pub(crate) fn cost(candidate: &LocatedText) -> usize {
    candidate.row.0.len() + candidate.row.3.len() + candidate.context.len() + 384
}
