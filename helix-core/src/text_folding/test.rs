use crate::graphemes::next_grapheme_boundary;

use super::*;

use test_utils::new_fold_points;
use test_utils::{fold_points, fold_points_filtered_by};
use test_utils::{folds_eq, folds_eq_by};
use test_utils::{FOLDED_TEXT_SAMPLE, TEXT_SAMPLE};

#[test]
fn fold_text() {
    _ = *FOLDED_TEXT_SAMPLE;
}

#[test]
fn fold_container_from() {
    let mut points = fold_points();
    // additional points will be removed (single-line targets get eliminated
    // because end block_line = target_line - 1 < start block_line)
    points.extend(
        [("rm", 73, 77..=77)]
            .into_iter()
            .map(|(object, header_line, target_lines)| {
                new_fold_points(*TEXT_SAMPLE, object, header_line, target_lines)
            }),
    );

    let container = FoldContainer::from(*TEXT_SAMPLE, points.clone());

    // Single-line target folds are removed (indices 0,1,8,16,17,18,20,21 from fold_points, plus "rm")
    let surviving_indices: Vec<usize> = vec![2, 3, 4, 5, 6, 7, 9, 10, 11, 12, 13, 14, 15, 19];
    assert_eq!(container.len(), surviving_indices.len());

    let partial_eq = |sfp1: &StartFoldPoint, sfp2: &StartFoldPoint| -> bool {
        sfp1.object == sfp2.object && sfp1.header == sfp2.header && sfp1.target == sfp2.target
    };
    assert!(container.start_points.iter().enumerate().all(|(i, sfp)| {
        let (expected, _) = &points[surviving_indices[i]];
        if partial_eq(sfp, expected) {
            return true;
        }
        eprintln!(
            "index = {i}\n\
            sfp = {sfp:#?}\n\
            expected = {expected:#?}"
        );
        false
    }));

    let partial_eq =
        |efp1: &EndFoldPoint, efp2: &EndFoldPoint| -> bool { efp1.target == efp2.target };
    assert!(container.end_points.iter().enumerate().all(|(i, efp)| {
        let (_, expected) = &points[surviving_indices[efp.link]];
        if partial_eq(efp, expected) {
            return true;
        }
        eprintln!(
            "index = {i}\n\
            efp = {efp:#?}\n\
            expected = {expected:#?}"
        );
        false
    }));
}

#[test]
fn fold_container_add() {
    let mut points = fold_points();
    points.extend([]);

    let container = &mut FoldContainer::from(
        *TEXT_SAMPLE,
        points
            .iter()
            .cloned()
            .enumerate()
            .filter(|(i, _)| i % 2 == 0)
            .map(|(_, points)| points)
            .collect(),
    );
    container.add(
        *TEXT_SAMPLE,
        points
            .iter()
            .cloned()
            .enumerate()
            .filter(|(i, _)| i % 2 != 0)
            .map(|(_, points)| points)
            .collect(),
    );

    let expected = &FoldContainer::from(*TEXT_SAMPLE, points);
    assert!(folds_eq(container, expected));
}

#[test]
fn fold_container_add_removes_partially_overlapping_fold() {
    let text = RopeSlice::from(
        "first {\n\
            a\n\
            second {\n\
                b\n\
            }\n\
        }\n",
    );
    let points = vec![
        new_fold_points(text, "first", 0, 1..=3),
        new_fold_points(text, "second", 2, 3..=5),
    ];

    let container = FoldContainer::from(text, points);

    assert_eq!(container.len(), 1);
    assert!(matches!(
        container.start_points()[0].object,
        FoldObject::TextObject("second")
    ));
    assert!(container.start_points()[0].is_superest());
}

#[test]
fn fold_container_remove_keeps_nested_links_valid() {
    let text = RopeSlice::from(
        "outer {\n\
            inner {\n\
                body\n\
            }\n\
        }\n",
    );
    let mut container = FoldContainer::from(
        text,
        vec![
            new_fold_points(text, "outer", 0, 1..=4),
            new_fold_points(text, "inner", 1, 2..=3),
        ],
    );

    let inner_idx = container
        .find(
            &FoldObject::TextObject("inner"),
            &(text.line_to_char(1)..=text.line_to_char(3)),
            |fold| fold.header()..=fold.end.target,
        )
        .expect("inner fold")
        .start_idx();
    container.remove(text, &[inner_idx]);

    assert_eq!(container.len(), 1);
    let fold = container.start_points()[0].fold(&container);
    assert!(matches!(fold.object(), FoldObject::TextObject("outer")));
    assert!(fold.is_superest());
    assert_eq!(
        container
            .find(
                &FoldObject::TextObject("outer"),
                &(fold.header()..=fold.end.target),
                |fold| fold.header()..=fold.end.target,
            )
            .map(|fold| fold.start_idx()),
        Some(0)
    );
}

#[test]
fn fold_container_remove_outer_promotes_inner_fold() {
    let text = RopeSlice::from(
        "outer {\n\
            inner {\n\
                body\n\
            }\n\
        }\n",
    );
    let mut container = FoldContainer::from(
        text,
        vec![
            new_fold_points(text, "outer", 0, 1..=4),
            new_fold_points(text, "inner", 1, 2..=3),
        ],
    );

    let outer_idx = container
        .find(
            &FoldObject::TextObject("outer"),
            &(container.start_points()[0].fold(&container).header()
                ..=container.start_points()[0].fold(&container).end.target),
            |fold| fold.header()..=fold.end.target,
        )
        .expect("outer fold")
        .start_idx();
    container.remove(text, &[outer_idx]);

    assert_eq!(container.len(), 1);
    let fold = container.start_points()[0].fold(&container);
    assert!(matches!(fold.object(), FoldObject::TextObject("inner")));
    assert!(fold.is_superest());
    assert_eq!(fold.start_idx(), 0);
}

#[test]
fn fold_container_replace() {
    // replacements, replaced
    let cases = [
        (&[0, 1][..], &[][..]),
        (&[2][..], &[3, 4, 5, 6, 7, 8][..]),
        (&[9][..], &[10, 11][..]),
        (&[12][..], &[13][..]),
        (&[14][..], &[15][..]),
        (&[19][..], &[16, 17, 18][..]),
    ];

    for (case_idx, (replacements, replaced)) in cases.into_iter().enumerate() {
        let container = &mut FoldContainer::from(
            *TEXT_SAMPLE,
            fold_points_filtered_by(|(i, _)| !replacements.contains(i)),
        );
        container.replace(
            *TEXT_SAMPLE,
            fold_points_filtered_by(|(i, _)| replacements.contains(i)),
        );

        let expected = &FoldContainer::from(
            *TEXT_SAMPLE,
            fold_points_filtered_by(|(i, _)| !replaced.contains(i)),
        );

        assert!(
            folds_eq_by(
                container,
                expected,
                |sfp1, sfp2| sfp1 == sfp2,
                |efp1, efp2| efp1.link == efp2.link,
            ),
            "case index = {case_idx}"
        );
    }
}

#[test]
fn fold_container_remove_by_selection() {
    // line from, line to, removed
    let cases = [
        (0, 0, &[][..]),
        (2, 3, &[][..]),
        (5, 6, &[][..]),
        (6, 7, &[][..]),
        (8, 8, &[2][..]),
        (17, 19, &[2, 4, 5][..]),
        (21, 34, &[2, 5, 6, 9, 10, 11][..]),
        (40, 42, &[12][..]),
        (45, 55, &[][..]),
    ];

    for (case_idx, (from, to, removed)) in cases.into_iter().enumerate() {
        let selection = &Selection::single(
            TEXT_SAMPLE.line_to_char(from),
            next_grapheme_boundary(*TEXT_SAMPLE, TEXT_SAMPLE.line_to_char(to)),
        );

        let container = &mut FoldContainer::from(*TEXT_SAMPLE, fold_points());
        container.remove_by_selection(*TEXT_SAMPLE, selection);

        let expected = &FoldContainer::from(
            *TEXT_SAMPLE,
            fold_points_filtered_by(|(i, _)| !removed.contains(i)),
        );

        assert!(folds_eq(container, expected), "case index = {case_idx}");
    }
}

#[test]
fn fold_container_throw_range_out_of_folds() {
    let container = &FoldContainer::from(*TEXT_SAMPLE, fold_points());

    // line from, line to, expected (line from, line to)
    let cases = [
        ((1, 1), Range::new(16, 31)),      // (1, 2)
        ((4, 4), Range::new(50, 65)),      // (4, 5)
        ((1, 4), Range::new(16, 65)),      // (1, 5)
        ((19, 63), Range::new(67, 842)),   // (6, 62)
        ((44, 10), Range::new(576, 67)),   // (39, 6)
        ((77, 45), Range::new(1027, 628)), // (72, 45)
    ];

    for (case_idx, ((from, to), expected)) in cases.into_iter().enumerate() {
        let range = Range::new(
            TEXT_SAMPLE.line_to_char(from),
            line_end_char_index(&TEXT_SAMPLE, to),
        );

        let result = container.throw_range_out_of_folds(*TEXT_SAMPLE, range);
        let expected = expected.with_direction(result.direction());

        assert_eq!(result, expected, "case index = {case_idx}");
    }
}

#[test]
fn fold_container_find() {
    let container = &FoldContainer::from(*TEXT_SAMPLE, fold_points());

    // object, block line range, expected
    let cases = [
        ("2", 8..=28, Some(0)),
        ("a", 8..=28, None),
        ("2", 8..=29, None),
        ("7", 28..=28, Some(5)),
        ("6", 20..=21, Some(4)),
        ("9", 32..=35, Some(6)),
        ("10", 33..=34, Some(7)),
    ];

    for (case_idx, (object, block, expected)) in cases.into_iter().enumerate() {
        let result = container.find(&FoldObject::TextObject(object), &block, |fold| {
            fold.start.line..=fold.end.line
        });
        let expected = expected.map(|idx| container.start_points[idx].fold(container));
        assert_eq!(result, expected, "case index = {case_idx}");
    }
}

#[test]
fn fold_container_start_points_in_range() {
    let container = &FoldContainer::from(*TEXT_SAMPLE, fold_points());

    // block line range, expected
    let cases = [
        (0..=0, None),
        (6..=40, Some(0..=8)),
        (10..=15, Some(1..=1)),
        (55..=70, Some(13..=13)),
        (0..=9, Some(0..=0)),
    ];

    for (case_idx, (block, expected)) in cases.into_iter().enumerate() {
        let result = container.start_points_in_range(&block, |sfp| sfp.line);
        let expected = expected.map_or(&[][..], |range| &container.start_points[range]);
        assert_eq!(result, expected, "case index = {case_idx}");
    }
}

#[test]
fn fold_container_fold_containing() {
    let container = &FoldContainer::from(*TEXT_SAMPLE, fold_points());

    // line, expected
    let cases = [
        (0, None),
        (1, None),
        (7, None),
        (11, Some(0)),
        (9, Some(0)),
        (57, None),
        (78, None),
        (12, Some(0)),
        (19, Some(3)),
    ];

    for (case_idx, (line, expected)) in cases.into_iter().enumerate() {
        let result = container.fold_containing(line, |fold| fold.start.line..=fold.end.line);
        let expected = expected.map(|idx| container.start_points[idx].fold(container));
        assert_eq!(result, expected, "case index = {case_idx}");
    }
}

#[test]
fn fold_container_superest_fold_containing() {
    let container = &FoldContainer::from(*TEXT_SAMPLE, fold_points());

    // line, expected
    let cases = [
        (0, None),
        (1, None),
        (7, None),
        (11, Some(0)),
        (9, Some(0)),
        (57, None),
        (78, None),
        (12, Some(0)),
        (19, Some(0)),
    ];

    for (case_idx, (line, expected)) in cases.into_iter().enumerate() {
        let result =
            container.superest_fold_containing(line, |fold| fold.start.line..=fold.end.line);
        let expected = expected.map(|idx| container.start_points[idx].fold(container));
        assert_eq!(result, expected, "case index = {case_idx}");
    }
}

#[test]
fn fold_annotations_folded_lines_between() {
    let container = &FoldContainer::from(*TEXT_SAMPLE, fold_points());
    let annotations = FoldAnnotations::new(Some(container));

    // line range, expected
    let cases = [
        (0..=0, 0),
        (3..=3, 0),
        (0..=3, 0),
        (0..=5, 0),
        (5..=7, 0),
        (5..=30, 21),
        (30..=31, 0),
        (30..=51, 10),
        (51..=51, 0),
        (62..=79, 1),
    ];

    for (case_idx, (line_range, expected)) in cases.into_iter().enumerate() {
        let result = annotations.folded_lines_between(&line_range);
        assert_eq!(result, expected, "case index = {case_idx}");
    }
}

#[test]
fn fold_container_update_by_transaction() {
    use crate::Rope;
    use crate::Transaction;
    use std::cell::RefCell;
    use std::iter::once;

    let init_container = &FoldContainer::from(*TEXT_SAMPLE, fold_points());
    let container = RefCell::new(FoldContainer::from(*TEXT_SAMPLE, fold_points()));

    let object_eq = |fold: Fold, object: &str| {
        matches!(
            fold.object(),
            FoldObject::TextObject(textobject) if *textobject == object
        )
    };

    let decrease_eq = |n: usize| n == init_container.len() - container.borrow().len();

    // a change, an assert function
    let cases: Vec<(_, Box<dyn Fn()>)> = vec![
        (
            // remove the first header char
            (0, 1, None),
            Box::new(|| {
                let container = container.borrow();
                let fold = container.start_points[0].fold(&container);

                assert!(
                    fold.header() == 66 && object_eq(fold, "2") && decrease_eq(0),
                    "fold = {fold:#?}"
                );
            }),
        ),
        (
            // replace the text "丂 line index: " from the 0i line
            (0, 15, Some("new header".into())),
            Box::new(|| {
                let container = container.borrow();
                let fold = container.start_points[0].fold(&container);

                assert!(object_eq(fold, "2") && decrease_eq(0), "fold = {fold:#?}");
            }),
        ),
        (
            // replace the trimmed 0i line
            (0, 16, Some("new header".into())),
            Box::new(|| {
                let container = container.borrow();
                let fold = container.start_points[0].fold(&container);

                assert!(object_eq(fold, "2") && decrease_eq(0), "fold = {fold:#?}");
            }),
        ),
        (
            // remove the entire 0i line
            (0, 17, None),
            Box::new(|| {
                let container = container.borrow();
                let fold = container.start_points[0].fold(&container);

                assert!(object_eq(fold, "2") && decrease_eq(0), "fold = {fold:#?}");
            }),
        ),
        (
            // remove the first nonwhitespace char of 11i line
            (137, 138, None),
            Box::new(|| {
                let container = container.borrow();
                let fold = container.start_points[1].fold(&container);

                assert!(object_eq(fold, "4") && decrease_eq(1), "fold = {fold:#?}");
            }),
        ),
        (
            // remove the last nonwhitespace char of the 19i line
            (263, 264, None),
            Box::new(|| {
                let container = container.borrow();
                let fold = container.start_points[2].fold(&container);

                assert!(object_eq(fold, "5") && decrease_eq(1), "fold = {fold:#?}");
            }),
        ),
        (
            // remove the 33i entire line
            (486, 504, None),
            Box::new(|| {
                let container = container.borrow();
                let fold = container.start_points[6].fold(&container);

                assert!(object_eq(fold, "9") && decrease_eq(2), "fold = {fold:#?}");
            }),
        ),
        (
            // remove the last nonwhitespace char of the 18i line
            (263, 264, None),
            Box::new(|| {
                let container = container.borrow();
                let fold = container.start_points[2].fold(&container);

                assert!(
                    object_eq(fold, "5") && fold.start.line == 19 && decrease_eq(1),
                    "fold = {fold:#?}"
                );
            }),
        ),
        (
            // remove the 9i entire line
            (117, 136, None),
            Box::new(|| {
                let container = container.borrow();
                let fold = container.start_points[3].fold(&container);

                assert!(object_eq(fold, "5") && decrease_eq(0), "fold = {fold:#?}");
            }),
        ),
        (
            // replace the text "19 乪\n\t" of the 19i-20i lines
            (279, 285, Some("new text\n\t".into())),
            Box::new(|| {
                let container = container.borrow();
                let fold = container.start_points[4].fold(&container);

                assert!(object_eq(fold, "6") && decrease_eq(0), "fold = {fold:#?}");
            }),
        ),
        (
            // replace the text "19 乪\n\t\tline" of the 19i-20i lines
            (279, 292, Some("new text\n\t\tnew text".into())),
            Box::new(|| {
                let container = container.borrow();
                let fold = container.start_points[4].fold(&container);

                assert!(object_eq(fold, "7") && decrease_eq(1), "fold = {fold:#?}");
            }),
        ),
        (
            // remove the line ending of the 55i line and 56i-57i lines
            (737, 740, None),
            Box::new(|| {
                let container = container.borrow();
                let fold = container.start_points[12].fold(&container);

                assert!(
                    object_eq(fold, "15") && decrease_eq(0) && fold.end.line == 54,
                    "fold = {fold:#?}"
                );
            }),
        ),
        (
            // remove the line ending of the 33i line
            (502, 503, None),
            Box::new(|| {
                let container = container.borrow();
                let fold = container.start_points[8].fold(&container);

                assert!(
                    object_eq(fold, "12") && decrease_eq(1) && fold.end.line == 43,
                    "fold = {fold:#?}"
                )
            }),
        ),
        (
            // remove the entire 39i-40i lines
            (558, 576, None),
            Box::new(|| {
                let container = container.borrow();
                let fold = container.start_points[9].fold(&container);

                assert!(
                    object_eq(fold, "13") && decrease_eq(1) && fold.is_superest(),
                    "fold = {fold:#?}"
                )
            }),
        ),
    ];

    for (change, assert) in cases {
        let doc = &mut Rope::from(*TEXT_SAMPLE);
        // reset container
        *container.borrow_mut() = init_container.clone();

        let transaction = &Transaction::change(doc, once(change));
        transaction.apply(doc);
        // update container
        container
            .borrow_mut()
            .update_by_transaction(doc.slice(..), *TEXT_SAMPLE, transaction);

        assert();
    }
}
