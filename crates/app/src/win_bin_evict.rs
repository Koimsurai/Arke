//! Making room in a full Recycle Bin the way Windows does (#379): the oldest items are deleted
//! for good, without asking, until the new one fits.
//!
//! The fast Recycle Bin first puts the new item in (a rename that can still be undone), then
//! deletes older items, oldest deletion time (from their `$I` records) first. Items the same
//! delete put there are never taken. Each one goes like "Delete permanently" on the Recycle Bin
//! page ([`crate::win_purge::purge_bin`]): its `$R` data first, its `$I` record only once that is
//! all gone, so an item that can't be deleted completely stays in the bin with what is left.
//! Only a log line says what was deleted: names in the bin, never where the items came from (logs
//! end up in issue reports).

use std::collections::HashSet;
use std::path::Path;

use crate::win_bin_usage::Candidate;
use crate::win_fast_recycle::{Head, VERSION, head};

/// Older items of one bin to delete for good, oldest first. Made from what the tracker knows the
/// first time a delete needs room, then used for the rest of that delete.
pub(crate) struct Evictor {
    /// Oldest deletion first; by name for the same time, so the order is always the same.
    queue: Vec<Candidate>,
    /// Next in `queue` to look at.
    next: usize,
}

impl Evictor {
    pub(crate) fn new(mut candidates: Vec<Candidate>) -> Self {
        candidates.sort_by(|a, b| a.deleted.cmp(&b.deleted).then_with(|| a.name.cmp(&b.name)));
        Self {
            queue: candidates,
            next: 0,
        }
    }

    /// Items not looked at yet, leaving out `mine` (this delete's own).
    fn left<'a>(&'a self, mine: &'a HashSet<String>) -> impl Iterator<Item = &'a Candidate> {
        self.queue
            .get(self.next..)
            .unwrap_or_default()
            .iter()
            .filter(move |c| !mine.contains(&c.name))
    }

    /// Whether deleting the items left, none of `mine`, could free `need` bytes.
    pub(crate) fn can_free(&self, need: u64, mine: &HashSet<String>) -> bool {
        let mut total = 0u64;
        for c in self.left(mine) {
            if total >= need {
                return true;
            }
            total = total.saturating_add(c.size);
        }
        total >= need
    }

    /// Deletes items oldest first with `delete` (true when it is gone) until `need` bytes are
    /// freed or none are left. Never one of `mine`. An item `delete` fails on is skipped, isn't
    /// counted, and the next one is tried.
    pub(crate) fn evict(
        &mut self,
        need: u64,
        mine: &HashSet<String>,
        mut delete: impl FnMut(&Candidate) -> bool,
    ) -> Evicted {
        let mut out = Evicted::default();
        while out.bytes < need {
            let Some(c) = self.queue.get(self.next) else {
                out.short = true;
                break;
            };
            self.next += 1;
            if mine.contains(&c.name) {
                continue;
            }
            if delete(c) {
                out.bytes = out.bytes.saturating_add(c.size);
                out.items.push(c.clone());
            } else {
                out.failed += 1;
            }
        }
        out
    }
}

/// What making room deleted, over one delete.
#[derive(Default)]
pub(crate) struct Evicted {
    pub(crate) items: Vec<Candidate>,
    pub(crate) bytes: u64,
    /// Items that couldn't be deleted (in use, changed since the bin was listed).
    pub(crate) failed: usize,
    /// Room couldn't be made: nothing left to delete. The bin stays over its maximum until a
    /// later delete makes room, as with Windows (it trims the bin on the next delete).
    pub(crate) short: bool,
}

/// Names shown in the log line, at most.
const LOGGED_NAMES: usize = 20;

impl Evicted {
    pub(crate) fn absorb(&mut self, more: Self) {
        self.items.extend(more.items);
        self.bytes = self.bytes.saturating_add(more.bytes);
        self.failed += more.failed;
        self.short |= more.short;
    }

    /// One log line for a delete's whole eviction on `bin`: how many, how much, how old, and
    /// their names in the bin (no original paths).
    pub(crate) fn log(&self, bin: &Path, now: i64) {
        if self.items.is_empty() && self.failed == 0 && !self.short {
            return;
        }
        let days = |filetime: i64| now.saturating_sub(filetime) / 864_000_000_000;
        let oldest = self.items.iter().map(|c| c.deleted).min().unwrap_or(now);
        let newest = self.items.iter().map(|c| c.deleted).max().unwrap_or(now);
        let names: Vec<String> = self
            .items
            .iter()
            .take(LOGGED_NAMES)
            .map(|c| data_name(&c.name))
            .collect();
        let more = self.items.len().saturating_sub(LOGGED_NAMES);
        log::info!(
            "recycle bin {}: deleted {} oldest items for good to make room ({} bytes, deleted {} to {} days ago): {}{}{}{}",
            bin.display(),
            self.items.len(),
            self.bytes,
            days(newest),
            days(oldest),
            names.join(", "),
            if more > 0 {
                format!(" and {more} more")
            } else {
                String::new()
            },
            if self.failed > 0 {
                format!("; {} could not be deleted", self.failed)
            } else {
                String::new()
            },
            if self.short {
                "; still over its maximum"
            } else {
                ""
            },
        );
    }
}

/// The `$R` name paired with a `$I` record's name.
fn data_name(info: &str) -> String {
    info.strip_prefix("$I")
        .map_or_else(|| info.to_owned(), |rest| format!("$R{rest}"))
}

/// Deletes one bin item for good, if its `$I` record is still the one planned on (the bin may
/// have changed since the tracker last saw it). True when it is gone.
pub(crate) fn delete_for_good(bin: &Path, c: &Candidate) -> bool {
    use crate::shell::{OpOutcome, OpProgress};
    let still = std::fs::read(bin.join(&c.name))
        .ok()
        .and_then(|bytes| head(&bytes));
    let planned = Head {
        version: VERSION,
        size: c.size,
        deleted: c.deleted,
    };
    if still != Some(planned) {
        return false;
    }
    let data = bin.join(data_name(&c.name));
    let item = crate::win_purge::BinItem {
        shown: data.clone(),
        data,
    };
    match crate::win_purge::purge_bin(&[item], &OpProgress::default()) {
        Ok(OpOutcome::Done) => {
            crate::win_bin_usage::tracker().note_removed(bin, &c.name);
            true
        }
        Ok(_) => false,
        Err(e) => {
            log::warn!(
                "recycle bin {}: {} not deleted to make room: {e}",
                bin.display(),
                data_name(&c.name)
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(name: &str, size: u64, deleted: i64) -> Candidate {
        Candidate {
            name: name.to_owned(),
            size,
            deleted,
        }
    }

    fn names(evicted: &Evicted) -> Vec<&str> {
        evicted.items.iter().map(|c| c.name.as_str()).collect()
    }

    #[test]
    fn the_oldest_go_first_until_there_is_room() {
        let mut evictor = Evictor::new(vec![
            item("$INEW", 10, 300),
            item("$IOLD", 10, 100),
            item("$IMID-B", 10, 200),
            item("$IMID-A", 10, 200),
        ]);
        let none = HashSet::new();
        let first = evictor.evict(15, &none, |_| true);
        assert_eq!(names(&first), ["$IOLD", "$IMID-A"], "by time, then name");
        assert_eq!(first.bytes, 20);
        assert!(!first.short);
        let second = evictor.evict(1, &none, |_| true);
        assert_eq!(
            names(&second),
            ["$IMID-B"],
            "a later item goes on from there"
        );
        assert!(
            evictor.evict(0, &none, |_| true).items.is_empty(),
            "nothing needed"
        );
    }

    #[test]
    fn items_of_the_same_delete_are_never_taken() {
        let candidates = vec![
            item("$IMINE1", 100, 1),
            item("$IOTHER", 10, 2),
            item("$IMINE2", 100, 3),
        ];
        let mine: HashSet<String> = ["$IMINE1", "$IMINE2"].map(String::from).into();
        let mut evictor = Evictor::new(candidates);
        assert!(evictor.can_free(10, &mine));
        assert!(
            !evictor.can_free(11, &mine),
            "this delete's own items don't count as room"
        );
        let mut asked = Vec::new();
        let evicted = evictor.evict(50, &mine, |c| {
            asked.push(c.name.clone());
            true
        });
        assert_eq!(asked, ["$IOTHER"]);
        assert_eq!(evicted.bytes, 10);
        assert!(evicted.short, "not enough room without them");
    }

    #[test]
    fn an_item_that_cant_be_deleted_is_skipped_and_not_counted() {
        let mut evictor = Evictor::new(vec![
            item("$IA", 10, 1),
            item("$IB", 10, 2),
            item("$IC", 10, 3),
        ]);
        let evicted = evictor.evict(10, &HashSet::new(), |c| c.name != "$IA");
        assert_eq!(names(&evicted), ["$IB"]);
        assert_eq!((evicted.bytes, evicted.failed), (10, 1));
    }

    #[test]
    fn room_is_known_before_anything_moves() {
        let evictor = Evictor::new(vec![item("$IA", 10, 1), item("$IB", 5, 2)]);
        let none = HashSet::new();
        assert!(evictor.can_free(0, &none));
        assert!(evictor.can_free(15, &none));
        assert!(!evictor.can_free(16, &none));
        assert!(!Evictor::new(Vec::new()).can_free(1, &none));
    }

    #[test]
    fn evictions_add_up_over_a_delete() {
        let mut all = Evicted::default();
        all.absorb(Evicted {
            items: vec![item("$IA", 3, 1)],
            bytes: 3,
            failed: 1,
            short: false,
        });
        all.absorb(Evicted {
            items: vec![item("$IB", 4, 2)],
            bytes: 4,
            failed: 0,
            short: true,
        });
        assert_eq!(names(&all), ["$IA", "$IB"]);
        assert_eq!((all.bytes, all.failed, all.short), (7, 1, true));
    }

    #[test]
    fn logs_name_the_bin_items() {
        assert_eq!(data_name("$IAB12CD.txt"), "$RAB12CD.txt");
        assert_eq!(data_name("odd"), "odd");
    }
}
