//! Unit tests for the SoarDB implementation

use clarity::types::chainstate::StacksBlockId;

use crate::{PutCommand, SoarDB};

/// use the current value in db to create a prior_value
///  for a put command
fn make_put(db: &SoarDB, k: &str, v: &str) -> PutCommand {
    let prior_value = db.get_value(k).unwrap();
    PutCommand {
        key: k.to_string(),
        prior_value,
        value: v.to_string(),
    }
}

/// Test basic usage of the db: a single chain of blocks
///  with k-v operations
#[test]
fn simple_storage_chain() {
    let mut db = SoarDB::new_memory();
    db.add_genesis(StacksBlockId([1; 32]), vec![make_put(&db, "A", "1")])
        .unwrap();
    assert_eq!(db.get_value("A"), Ok(Some("1".into())));

    db.add_block_ops(
        StacksBlockId([2; 32]),
        StacksBlockId([1; 32]),
        vec![
            make_put(&db, "B", "2"),
            make_put(&db, "C", "2"),
            make_put(&db, "D", "2"),
        ],
    )
    .unwrap();

    assert_eq!(db.get_value("A"), Ok(Some("1".into())));
    assert_eq!(db.get_value("B"), Ok(Some("2".into())));
    assert_eq!(db.get_value("C"), Ok(Some("2".into())));
    assert_eq!(db.get_value("D"), Ok(Some("2".into())));

    db.add_block_ops(
        StacksBlockId([3; 32]),
        StacksBlockId([2; 32]),
        vec![
            make_put(&db, "B", "3"),
            make_put(&db, "C", "3"),
            make_put(&db, "D", "3"),
        ],
    )
    .unwrap();

    assert_eq!(db.get_value("A"), Ok(Some("1".into())));
    assert_eq!(db.get_value("B"), Ok(Some("3".into())));
    assert_eq!(db.get_value("C"), Ok(Some("3".into())));
    assert_eq!(db.get_value("D"), Ok(Some("3".into())));
}

/// Test forking from a longer chain (1 -> 2 -> 3 -> 4)
///  to a shorter chain (1 -> 2 -> 3) and then back again
#[test]
fn fork_to_shorter_chain() {
    let mut db = SoarDB::new_memory();
    db.add_genesis(StacksBlockId([1; 32]), vec![make_put(&db, "A", "1")])
        .unwrap();
    assert_eq!(db.get_value("A"), Ok(Some("1".into())));

    db.add_block_ops(
        StacksBlockId([2; 32]),
        StacksBlockId([1; 32]),
        vec![
            make_put(&db, "B", "2"),
            make_put(&db, "C", "2"),
            make_put(&db, "D", "2"),
        ],
    )
    .unwrap();

    assert_eq!(db.get_value("A"), Ok(Some("1".into())));
    assert_eq!(db.get_value("B"), Ok(Some("2".into())));
    assert_eq!(db.get_value("C"), Ok(Some("2".into())));
    assert_eq!(db.get_value("D"), Ok(Some("2".into())));

    // these puts will be applied in a different fork
    let fork_ops = vec![
        make_put(&db, "B", "f3"),
        make_put(&db, "E", "f3"),
        make_put(&db, "A", "f3"),
    ];

    db.add_block_ops(
        StacksBlockId([3; 32]),
        StacksBlockId([2; 32]),
        vec![
            make_put(&db, "B", "3"),
            make_put(&db, "C", "3"),
            make_put(&db, "Z", "3"),
        ],
    )
    .unwrap();

    assert_eq!(db.get_value("A"), Ok(Some("1".into())));
    assert_eq!(db.get_value("B"), Ok(Some("3".into())));
    assert_eq!(db.get_value("C"), Ok(Some("3".into())));
    assert_eq!(db.get_value("D"), Ok(Some("2".into())));
    assert_eq!(db.get_value("Z"), Ok(Some("3".into())));
    assert_eq!(db.get_value("E"), Ok(None));

    db.add_block_ops(
        StacksBlockId([4; 32]),
        StacksBlockId([3; 32]),
        vec![
            make_put(&db, "B", "4"),
            make_put(&db, "C", "4"),
            make_put(&db, "D", "4"),
        ],
    )
    .unwrap();

    assert_eq!(db.get_value("A"), Ok(Some("1".into())));
    assert_eq!(db.get_value("B"), Ok(Some("4".into())));
    assert_eq!(db.get_value("C"), Ok(Some("4".into())));
    assert_eq!(db.get_value("D"), Ok(Some("4".into())));
    assert_eq!(db.get_value("Z"), Ok(Some("3".into())));
    assert_eq!(db.get_value("E"), Ok(None));

    // these ops will be applied when we fork back
    let fork_back_ops = vec![make_put(&db, "C", "5")];

    db.add_block_ops(StacksBlockId([13; 32]), StacksBlockId([2; 32]), fork_ops)
        .unwrap();

    assert_eq!(db.get_value("A"), Ok(Some("f3".into())));
    assert_eq!(db.get_value("B"), Ok(Some("f3".into())));
    assert_eq!(db.get_value("E"), Ok(Some("f3".into())));
    assert_eq!(db.get_value("C"), Ok(Some("2".into())));
    assert_eq!(db.get_value("D"), Ok(Some("2".into())));
    assert_eq!(db.get_value("Z"), Ok(None));

    db.add_block_ops(
        StacksBlockId([5; 32]),
        StacksBlockId([4; 32]),
        fork_back_ops,
    )
    .unwrap();

    assert_eq!(db.get_value("A"), Ok(Some("1".into())));
    assert_eq!(db.get_value("B"), Ok(Some("4".into())));
    assert_eq!(db.get_value("C"), Ok(Some("5".into())));
    assert_eq!(db.get_value("D"), Ok(Some("4".into())));
    assert_eq!(db.get_value("Z"), Ok(Some("3".into())));
    assert_eq!(db.get_value("E"), Ok(None));
}
