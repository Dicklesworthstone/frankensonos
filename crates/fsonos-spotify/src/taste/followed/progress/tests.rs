use super::*;

const OWNER: &str = "followed-proof-owner";
const GENERATION: u64 = 7;
const NOW: i64 = 1_000_000;

fn id(position: u32) -> String {
    format!("FollowedArtist{position:06}")
}

fn row(position: u32) -> Row {
    Row {
        id: id(position),
        name: format!("Artist {position}"),
        genres: vec!["jazz".into()],
    }
}

fn page(first: u32, total: u32) -> Page {
    let end = first.saturating_add(PAGE_LIMIT).min(total);
    Page {
        after: first.checked_sub(1).map(id),
        next: (end < total).then(|| id(end - 1)),
        total: Some(total),
        items: (first..end).map(row).collect(),
    }
}

fn complete(artists: u32) -> Progress {
    let mut progress = Progress::new(OWNER, GENERATION, NOW);
    progress.nonce = "test-consumer-receipt".into();
    if artists == 0 {
        progress.pages.push(page(0, 0));
    } else {
        progress.pages = (0..artists)
            .step_by(PAGE_LIMIT as usize)
            .map(|first| page(first, artists))
            .collect();
    }
    progress.verify = progress.pages.len() > super::super::FOLLOWED_ARTIST_PAGE_BUDGET;
    progress
}

fn encoded(progress: &Progress) -> String {
    serde_json::to_string(progress).unwrap()
}

fn valid(progress: &Progress) -> bool {
    Progress::decode(&encoded(progress), OWNER, GENERATION, NOW).is_some()
}

#[test]
fn checkpoint_reuse_requires_the_same_account_adoption_and_recent_evidence() {
    let progress = complete(75);
    let encoded = encoded(&progress);
    assert!(Progress::decode(&encoded, OWNER, GENERATION, NOW).is_some());
    assert!(Progress::decode(&encoded, "another-owner", GENERATION, NOW).is_none());
    assert!(Progress::decode(&encoded, OWNER, GENERATION + 2, NOW).is_none());
    assert!(Progress::decode(&encoded, OWNER, GENERATION, NOW - 1).is_none());
    assert!(Progress::decode(&encoded, OWNER, GENERATION, NOW + MAX_AGE).is_some());
    assert!(Progress::decode(&encoded, OWNER, GENERATION, NOW + MAX_AGE + 1).is_none());

    let mut invalid = progress.clone();
    invalid.version += 1;
    assert!(!valid(&invalid));
    for nonce in ["".into(), "invalid\nclaim".into(), "x".repeat(129)] {
        let mut invalid = progress.clone();
        invalid.nonce = nonce;
        assert!(!valid(&invalid));
    }
    let mut invalid = progress.clone();
    invalid.pages[1].after = Some(id(0));
    assert!(
        !valid(&invalid),
        "a stored cursor must continue its prior page"
    );
    let mut invalid = progress;
    invalid.pages[1].items[0] = invalid.pages[0].items[0].clone();
    assert!(
        !valid(&invalid),
        "a stored duplicate cannot prove full membership"
    );
}

#[test]
fn impossible_verification_states_cannot_authorize_completed_taste() {
    let small = complete(75);
    assert!(small.ready());
    assert!(valid(&small));
    let mut invalid = small.clone();
    invalid.verified = 1;
    assert!(
        !valid(&invalid),
        "verification needs an explicit resumed-read state"
    );
    let mut invalid = small;
    invalid.limited = true;
    assert!(
        !valid(&invalid),
        "terminal limits cannot retain publishable pages"
    );

    let mut resumed = complete(
        (u32::try_from(super::super::FOLLOWED_ARTIST_PAGE_BUDGET).unwrap() + 1) * PAGE_LIMIT,
    );
    assert!(resumed.read_complete());
    assert!(!resumed.ready());
    assert!(valid(&resumed));
    let mut invalid = resumed.clone();
    invalid.verify = false;
    assert!(
        !valid(&invalid),
        "a directory larger than one window must be verified"
    );
    let mut invalid = resumed.clone();
    invalid.applied = true;
    assert!(
        !valid(&invalid),
        "unchecked membership cannot be acknowledged"
    );
    let mut invalid = resumed.clone();
    invalid.verified = invalid.pages.len() + 1;
    assert!(!valid(&invalid));
    let mut invalid = resumed.clone();
    invalid.pages.pop();
    invalid.verified = 1;
    assert!(
        !valid(&invalid),
        "verification cannot advance before reading ends"
    );

    resumed.verified = resumed.pages.len();
    assert!(resumed.ready());
    assert!(valid(&resumed));
    resumed.applied = true;
    assert!(valid(&resumed));
    let report = resumed.report(true);
    assert!(!report.pending);
    assert_eq!(report.phase, FollowedArtistPhase::Ready);
    assert_eq!(report.verified_pages, resumed.pages.len());
}

#[test]
fn rejected_pages_do_not_advance_the_cursor_or_poison_an_accepted_prefix() {
    let first = page(0, 150);
    let next = page(50, 150);
    let mut index = Index::default();
    index.accept(&first).unwrap();
    let mut wrong_cursor = next.clone();
    wrong_cursor.after = Some(id(25));
    let mut repeated_artist = next.clone();
    repeated_artist.items[0] = first.items[10].clone();
    let mut repeated_within_page = next.clone();
    repeated_within_page.items[1] = repeated_within_page.items[0].clone();
    let mut skipped_cursor = next.clone();
    skipped_cursor.next = Some(id(120));
    let mut repeated_cursor = next.clone();
    repeated_cursor.next.clone_from(&first.next);
    let mut changed_total = next.clone();
    changed_total.total = Some(151);
    let mut early_end = next.clone();
    early_end.next = None;
    let mut empty_middle = next.clone();
    empty_middle.items.clear();
    for invalid in [
        wrong_cursor,
        repeated_artist,
        repeated_within_page,
        skipped_cursor,
        repeated_cursor,
        changed_total,
        early_end,
        empty_middle,
    ] {
        assert!(index.accept(&invalid).is_err(), "{invalid:?}");
        assert_eq!(index.pages, 1);
        assert_eq!(index.artists.len(), 50);
        assert_eq!(index.next, first.next);
        assert_eq!(index.total, Some(150));
    }
    index.accept(&next).unwrap();
    index.accept(&page(100, 150)).unwrap();
    assert_eq!(index.artists.len(), 150);
    assert_eq!(index.next, None);
    assert!(
        index.accept(&page(150, 200)).is_err(),
        "a terminal page ends the chain"
    );
}

#[test]
fn optional_totals_do_not_erase_previously_proved_coverage() {
    let mut index = Index::default();
    index.accept(&page(0, 150)).unwrap();
    let mut next = page(50, 150);
    next.total = None;
    index.accept(&next).unwrap();
    assert_eq!(index.total, Some(150));
    let mut shortened = page(100, 125);
    shortened.total = None;
    assert!(index.accept(&shortened).is_err());
    assert_eq!(index.artists.len(), 100);
    let mut last = page(100, 150);
    last.total = None;
    index.accept(&last).unwrap();
    assert_eq!(index.artists.len(), 150);
    assert_eq!(index.next, None);

    let empty = complete(0);
    assert!(empty.ready());
    assert!(valid(&empty));
    assert_eq!(empty.report(true).artists, 0);
}

#[test]
fn untrusted_artist_metadata_and_cursor_values_are_rejected() {
    for value in [
        "".into(),
        "Artist/other".into(),
        "Artist\n".into(),
        "x".repeat(129),
    ] {
        assert!(!cursor(&value));
    }
    assert!(cursor("ValidArtist123"));
    let first = page(0, 50);
    for change in 0..6 {
        let mut invalid = first.clone();
        match change {
            0 => invalid.items[0].id.clear(),
            1 => invalid.items[0].name = " \t".into(),
            2 => invalid.items[0].name = "x".repeat(16 * 1024 + 1),
            3 => invalid.items[0].genres = vec!["jazz".into(); 257],
            4 => invalid.items[0].genres = vec!["x".repeat(1_025)],
            _ => invalid.items[0].genres = vec!["jazz\n".into()],
        }
        assert!(Index::default().accept(&invalid).is_err());
    }
}

#[test]
fn metadata_and_entry_limits_become_small_valid_terminal_checkpoints() {
    for metadata in [false, true] {
        let artists = if metadata {
            1_024
        } else {
            MAX_FOLLOWED_ARTISTS + 1
        };
        let mut progress = complete(artists);
        if metadata {
            for page in &mut progress.pages {
                for item in &mut page.items {
                    item.name = "x".repeat(16 * 1024);
                }
            }
        }
        let pages = progress.pages.len();
        let encoded = progress.encode().unwrap();
        assert!(encoded.len() < 1_024);
        let decoded = Progress::decode(&encoded, OWNER, GENERATION, NOW).unwrap();
        assert!(decoded.limited);
        assert!(decoded.pages.is_empty());
        assert!(!decoded.ready());
        let report = decoded.report(true);
        assert!(report.limited);
        assert!(!report.pending);
        assert_eq!(report.pages, pages);
        assert_eq!(report.artists, artists as usize);
        assert_eq!(report.verified_pages, 0);
    }
}
