use super::*;
use crate::engine::verification::sql_tables::reader::Rows;

fn stats(rows: &[[i64; 4]]) -> Table {
    let mut result = Rows::new();
    for row in rows {
        *result
            .entry(row.iter().map(|v| Cell::Integer(*v)).collect())
            .or_default() += 1;
    }
    Table {
        columns: ["item_pid", PLAY, SKIP, "play_count_user"]
            .map(String::from)
            .to_vec(),
        definitions: Vec::new(),
        rows: result,
    }
}

#[test]
fn positive_counters_qualify_and_may_be_kept_or_reset_per_row() {
    let budget = Budget::new();
    let before = stats(&[[1, 3, 1, 9], [2, 2, 0, 4], [3, 0, 0, 1]]);
    qualify(&before).unwrap();
    let after = stats(&[[1, 0, 0, 9], [2, 2, 0, 4], [3, 0, 0, 1]]);
    let mut expected = before.clone();
    let resets = absorb(&mut expected, &after, &budget).unwrap();
    assert_eq!(expected, after);
    assert_eq!(
        resets,
        [Reset {
            item_pid: 1,
            previous_play: 3,
            previous_skip: 1,
            play: 0,
            skip: 0
        }]
    );
    // Only one of the two counters may reset, too.
    let partial = stats(&[[1, 3, 0, 9], [2, 2, 0, 4], [3, 0, 0, 1]]);
    let mut expected = before.clone();
    assert_eq!(absorb(&mut expected, &partial, &budget).unwrap().len(), 1);
    assert_eq!(expected, partial);
}

#[test]
fn any_other_statistics_change_is_left_for_the_exact_comparison() {
    let budget = Budget::new();
    let before = stats(&[[1, 3, 1, 9], [2, 0, 0, 4]]);
    for after in [
        // Lifetime count changed alongside the reset.
        stats(&[[1, 0, 0, 8], [2, 0, 0, 4]]),
        // A counter rose, or fell to a nonzero value.
        stats(&[[1, 4, 1, 9], [2, 0, 0, 4]]),
        stats(&[[1, 2, 1, 9], [2, 0, 0, 4]]),
        // A zero counter cannot "reset" into a change elsewhere.
        stats(&[[1, 3, 1, 9], [2, 0, 0, 5]]),
    ] {
        let mut expected = before.clone();
        assert!(absorb(&mut expected, &after, &budget).unwrap().is_empty());
        assert_eq!(expected, before);
        assert_ne!(expected, after);
    }
}

#[test]
fn duplicates_and_negative_counters_refuse_qualification() {
    let budget = Budget::new();
    assert!(qualify(&stats(&[[1, -1, 0, 0]])).is_err());
    assert!(qualify(&stats(&[[1, 0, 0, 0], [1, 0, 0, 0]])).is_err());
    assert!(qualify(&stats(&[[1, 0, 0, 0], [1, 2, 0, 0]])).is_err());
    // An ambiguous observed identity is never used to approve a reset.
    let before = stats(&[[1, 3, 0, 9]]);
    let mut expected = before.clone();
    let ambiguous = stats(&[[1, 0, 0, 9], [1, 0, 0, 9]]);
    assert!(
        absorb(&mut expected, &ambiguous, &budget)
            .unwrap()
            .is_empty()
    );
    assert_eq!(expected, before);
}
